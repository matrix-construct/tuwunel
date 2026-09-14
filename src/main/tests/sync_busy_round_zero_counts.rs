#![cfg(test)]

use std::{fs::remove_dir_all, net::TcpListener, time::Duration};

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{EventId, OwnedEventId, RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::client::{Client, field, poll_until, register, wait_until_ready};

mod client;

/// How long a notification count is polled for before the case gives up.
const COUNT_DEADLINE: Duration = Duration::from_secs(5);

const READER_TOKEN: &str = "sync-busy-round-zero-counts-reader-token";
const SENDER_TOKEN: &str = "sync-busy-round-zero-counts-sender-token";

/// Drives the busy-round notification reset over the client API.
///
/// A read receipt clears the reader's count, then a reaction gives the next
/// round a timeline without notifying anyone. That round must still carry an
/// explicit zero, which the handler can only authorize by loading the read
/// cursor whether or not the timeline is empty.
#[test]
fn busy_round_reports_the_zeroed_notification_count() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let db_path = Args::test_database_path("sync-busy-round");

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option(format!("database_path={db_path:?}"))
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run_result, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	drop(runtime);
	remove_dir_all(&db_path).ok();

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let reader_id = register(services, "busyroundreader", READER_TOKEN).await?;

	register(services, "busyroundsender", SENDER_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let sender = Client { services, base, token: SENDER_TOKEN };
	let room = sender
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	reader.join(&room).await?;

	let opening = reader.sync(None).await?;
	let since = next_batch(&opening)?;
	let body = json!({ "msgtype": "m.text", "body": "unread" });
	let message = sender
		.send(&room, "m.room.message", "message", &body)
		.await?;

	count_settles(services, &reader_id, &room, 1)
		.await
		.into_option()
		.ok_or_else(|| err!("the message did not notify the reader"))?;

	let notifying = reader.sync(Some(since)).await?;
	let since = next_batch(&notifying)?;

	notification_count(&notifying, &room, "notifying")?
		.eq(&1)
		.into_option()
		.ok_or_else(|| err!("the notifying round withheld the reader's one notification"))?;

	reader.read(&room, &message).await?;

	count_settles(services, &reader_id, &room, 0)
		.await
		.into_option()
		.ok_or_else(|| err!("the read receipt did not clear the notification count"))?;

	let annotation = json!({
		"m.relates_to": {
			"rel_type": "m.annotation",
			"event_id": message,
			"key": "\u{1f44d}",
		},
	});

	sender
		.send(&room, "m.reaction", "reaction", &annotation)
		.await?;

	let busy = reader.sync(Some(since)).await?;

	carries_reaction(&busy, &room)?
		.into_option()
		.ok_or_else(|| err!("the reset round carried no reaction, so it was not busy"))?;

	notification_count(&busy, &room, "busy reset")?
		.eq(&0)
		.into_option()
		.ok_or_else(|| err!("the busy reset round withheld the zeroed notification count"))?;

	Ok(())
}

#[implement(Client, params = "<'_>")]
async fn join(&self, room_id: &RoomId) -> Result {
	self.services
		.client
		.clients
		.default
		.post(self.url(&format!("rooms/{room_id}/join")))
		.bearer_auth(self.token)
		.json(&json!({}))
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

/// One non-blocking sync, optionally resuming from a token.
///
/// Every round the case reads is driven with `timeout=0` and an explicit
/// token, so the window each response covers is the one the preceding steps
/// wrote into.
#[implement(Client, params = "<'_>")]
async fn sync(&self, since: Option<&str>) -> Result<Value> {
	let path = since.map_or_else(
		|| "sync?timeout=0".to_owned(),
		|since| format!("sync?timeout=0&since={since}"),
	);

	self.services
		.client
		.clients
		.default
		.get(self.url(&path))
		.bearer_auth(self.token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await
		.map_err(Into::into)
}

fn next_batch(response: &Value) -> Result<&str> { field(response, "next_batch") }

#[implement(Client, params = "<'_>")]
async fn send(
	&self,
	room_id: &RoomId,
	event_type: &str,
	txn_id: &str,
	content: &Value,
) -> Result<OwnedEventId> {
	let path = format!("rooms/{room_id}/send/{event_type}/busy-round-{txn_id}");
	let response: Value = self
		.services
		.client
		.clients
		.default
		.put(self.url(&path))
		.bearer_auth(self.token)
		.json(content)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(field(&response, "event_id")?.try_into()?)
}

/// Whether the room's notification count settles on `want` before the
/// deadline.
///
/// Push evaluation trails the send that triggers it, so neither the count the
/// message raises nor the zero the receipt writes is readable from a single
/// sample.
async fn count_settles(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	want: u64,
) -> bool {
	poll_until(COUNT_DEADLINE, async || {
		services
			.pusher
			.notification_count(user_id, room_id)
			.await
			.eq(&want)
	})
	.await
}

/// The room's reported notification count on one sync round.
///
/// An absent count is the defect under test rather than a shape to tolerate,
/// so it fails the case here instead of reading as a zero, and `round` names
/// which of the two reads it was.
fn notification_count(response: &Value, room_id: &RoomId, round: &str) -> Result<u64> {
	joined_room(response, room_id)?
		.get("unread_notifications")
		.and_then(|unread| unread.get("notification_count"))
		.and_then(Value::as_u64)
		.ok_or_else(|| err!("the {round} round omitted the notification count for {room_id}"))
}

#[implement(Client, params = "<'_>")]
async fn read(&self, room_id: &RoomId, event_id: &EventId) -> Result {
	self.services
		.client
		.clients
		.default
		.post(self.url(&format!("rooms/{room_id}/receipt/m.read/{event_id}")))
		.bearer_auth(self.token)
		.json(&json!({}))
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

fn carries_reaction(response: &Value, room_id: &RoomId) -> Result<bool> {
	let events = joined_room(response, room_id)?
		.get("timeline")
		.and_then(|timeline| timeline.get("events"))
		.and_then(Value::as_array)
		.ok_or_else(|| err!("joined room {room_id} omitted its timeline"))?;

	let reacted = events
		.iter()
		.filter_map(|event| event.get("type").and_then(Value::as_str))
		.any(|event_type| event_type == "m.reaction");

	Ok(reacted)
}

fn joined_room<'a>(response: &'a Value, room_id: &RoomId) -> Result<&'a Value> {
	response["rooms"]["join"]
		.get(room_id.as_str())
		.ok_or_else(|| err!("sync response omitted joined room {room_id}"))
}
