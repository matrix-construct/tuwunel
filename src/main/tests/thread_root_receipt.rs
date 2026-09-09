#![cfg(test)]

use std::{
	env::var, fs::remove_dir_all, net::TcpListener, path::PathBuf, process::id as process_id,
	time::Duration,
};

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err, implement,
	ruma::{EventId, OwnedEventId, RoomId, UserId},
	utils::{BoolExt, ReadyExt},
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
	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path = DatabasePath(
		PathBuf::from(root).join(format!("tuwunel-thread-root-receipt-{}", process_id())),
	);

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

	let _reply_b = sender
		.reply(&room, "reply-b", &root_b, &reader_id)
		.await?;

	let _main = sender
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
		.await != main_before
		|| services
			.pusher
			.thread_notification_counts(&reader_id, &room)
			.await != threads_before
		|| services
			.pusher
			.thread_last_notification_reads(&reader_id, &room)
			.await != watermarks_before
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

	reader
		.receipt(&room, &reply_a, Some(&root_a))
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
		.await != main_before
		|| threads_after.get(&root_a) != Some(&(0, 0))
		|| threads_after.get(&root_b) != threads_before.get(&root_b)
		|| !watermarks_after.contains_key(&root_a)
		|| watermarks_after.get(&root_b) != watermarks_before.get(&root_b)
	{
		return Err!("an ordinary thread receipt reset the wrong unread state");
	}

	Ok(())
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
