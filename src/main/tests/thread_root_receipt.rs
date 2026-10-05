#![cfg(test)]

use std::{fs::remove_dir_all, net::TcpListener, path::PathBuf, time::Duration};

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err, implement,
	ruma::{EventId, OwnedEventId, RoomId, UserId},
	utils::{BoolExt, ReadyExt, future::ReadyEqExt},
};
use tuwunel_service::Services;

use self::client::{Client, field, poll_until, register, wait_until_ready};

mod client;

const COUNT_DEADLINE: Duration = Duration::from_secs(5);
const READER_TOKEN: &str = "thread-root-receipt-reader-token";
const SENDER_TOKEN: &str = "thread-root-receipt-sender-token";

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn thread_root_receipt_preserves_unread_replies() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let db_path = DatabasePath(Args::test_database_path("thread-root-receipt"));

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option(format!("database_path={:?}", db_path.0))
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

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let reader_id = register(services, "threadrootreader", READER_TOKEN).await?;

	register(services, "threadrootsender", SENDER_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let sender = Client { services, base, token: SENDER_TOKEN };
	let room = sender
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	reader.join(&room).await?;

	let root_a = sender.message(&room, "root-a", "root a").await?;
	let root_b = sender.message(&room, "root-b", "root b").await?;

	reader.receipt(&room, &root_b, None).await?;

	let reply_a = sender
		.reply(&room, "reply-a", &root_a, &reader_id)
		.await?;

	let reply_b = sender
		.reply(&room, "reply-b", &root_b, &reader_id)
		.await?;

	let main = sender
		.message(&room, "main", "main unread")
		.await?;

	counts_settle(services, &reader_id, &room, &root_a, &root_b).await?;

	let main_before = services
		.pusher
		.notification_count(&reader_id, &room)
		.await;

	let threads_before = services
		.pusher
		.thread_notification_counts(&reader_id, &room)
		.await;

	let watermarks_before = services
		.pusher
		.thread_last_notification_reads(&reader_id, &room)
		.await;

	let receipt_since = services.globals.current_count();

	reader
		.receipt(&room, &root_a, Some(&root_a))
		.await?;

	if services
		.pusher
		.notification_count(&reader_id, &room)
		.ne(&main_before)
		.await
		|| services
			.pusher
			.thread_notification_counts(&reader_id, &room)
			.ne(&threads_before)
			.await
		|| services
			.pusher
			.thread_last_notification_reads(&reader_id, &room)
			.ne(&watermarks_before)
			.await
	{
		return Err!("a receipt on the thread root changed unread state");
	}

	let root_stored = services
		.read_receipt
		.readreceipts_since(&room, receipt_since, None)
		.ready_any(|(_, _, event)| {
			let json = event.json().get();

			json.contains(root_a.as_str()) && json.contains("thread_id")
		})
		.await;

	if !root_stored {
		return Err!("the advancing thread-root receipt was not stored");
	}

	thread_receipt_marks_only_its_notifications_read(
		&reader, &room, &root_a, &reply_a, &reply_b, &main,
	)
	.await?;

	let threads_after = services
		.pusher
		.thread_notification_counts(&reader_id, &room)
		.await;

	let watermarks_after = services
		.pusher
		.thread_last_notification_reads(&reader_id, &room)
		.await;

	if services
		.pusher
		.notification_count(&reader_id, &room)
		.ne(&main_before)
		.await
		|| threads_after.get(&root_a) != Some(&(0, 0))
		|| threads_after.get(&root_b) != threads_before.get(&root_b)
		|| !watermarks_after.contains_key(&root_a)
		|| watermarks_after.get(&root_b) != watermarks_before.get(&root_b)
	{
		return Err!("an ordinary thread receipt reset the wrong unread state");
	}

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn thread_receipt_marks_only_its_notifications_read(
	reader: &Client<'_>,
	room: &RoomId,
	root_a: &EventId,
	reply_a: &EventId,
	reply_b: &EventId,
	main: &EventId,
) -> Result {
	let before = reader.notifications().await?;

	if notification_read(&before, reply_a)? {
		return Err!("thread A notification was already read before its receipt");
	}

	reader
		.receipt(room, reply_a, Some(root_a))
		.await?;

	let after = reader.notifications().await?;

	if !notification_read(&after, reply_a)? {
		return Err!("thread A notification was not read after its receipt");
	}

	for event in [reply_b, main] {
		if notification_read(&after, event)? != notification_read(&before, event)? {
			return Err!("thread A receipt changed an unrelated notification: {event}");
		}
	}

	Ok(())
}

fn notification_read(response: &Value, event_id: &EventId) -> Result<bool> {
	let matches_event = |notification: &&Value| {
		notification
			.get("event")
			.and_then(|event| event.get("event_id"))
			.and_then(Value::as_str)
			.eq(&Some(event_id.as_str()))
	};

	response
		.get("notifications")
		.and_then(Value::as_array)
		.and_then(|notifications| notifications.iter().find(matches_event))
		.and_then(|notification| notification.get("read"))
		.and_then(Value::as_bool)
		.ok_or_else(|| err!("notification missing for {event_id}"))
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "trace", skip_all)]
async fn notifications(&self) -> Result<Value> {
	self.services
		.client
		.clients
		.default
		.get(self.url("notifications?limit=100"))
		.bearer_auth(self.token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await
		.map_err(Into::into)
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "trace", skip_all)]
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

#[implement(Client, params = "<'_>")]
async fn message(&self, room_id: &RoomId, txn: &str, body: &str) -> Result<OwnedEventId> {
	self.send(room_id, txn, &json!({ "msgtype": "m.text", "body": body }))
		.await
}

#[implement(Client, params = "<'_>")]
async fn reply(
	&self,
	room_id: &RoomId,
	txn: &str,
	root: &EventId,
	mentioned: &UserId,
) -> Result<OwnedEventId> {
	let content = json!({
		"msgtype": "m.text",
		"body": "thread reply",
		"m.mentions": { "user_ids": [mentioned] },
		"m.relates_to": {
			"rel_type": "m.thread",
			"event_id": root,
			"is_falling_back": true,
			"m.in_reply_to": { "event_id": root },
		},
	});

	self.send(room_id, txn, &content).await
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "trace", skip_all)]
async fn send(&self, room_id: &RoomId, txn: &str, content: &Value) -> Result<OwnedEventId> {
	let response: Value = self
		.services
		.client
		.clients
		.default
		.put(self.url(&format!("rooms/{room_id}/send/m.room.message/{txn}")))
		.bearer_auth(self.token)
		.json(content)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(field(&response, "event_id")?.try_into()?)
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "trace", skip_all)]
async fn receipt(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	thread_root: Option<&EventId>,
) -> Result {
	let body = thread_root.map_or_else(|| json!({}), |root| json!({ "thread_id": root }));

	self.services
		.client
		.clients
		.default
		.post(self.url(&format!("rooms/{room_id}/receipt/m.read/{event_id}")))
		.bearer_auth(self.token)
		.json(&body)
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn counts_settle(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	root_a: &EventId,
	root_b: &EventId,
) -> Result {
	let settled = poll_until(COUNT_DEADLINE, async || {
		let main = services
			.pusher
			.notification_count(user_id, room_id)
			.await;

		let threads = services
			.pusher
			.thread_notification_counts(user_id, room_id)
			.await;

		main == 1
			&& threads
				.get(root_a)
				.is_some_and(|(count, highlight)| *count == 1 && *highlight > 0)
			&& threads
				.get(root_b)
				.is_some_and(|(count, highlight)| *count == 1 && *highlight > 0)
	})
	.await;

	settled
		.into_option()
		.ok_or_else(|| err!("notification counts did not settle before the receipt checks"))
}
