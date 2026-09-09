#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err, implement,
	ruma::{EventId, OwnedEventId, RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const IGNORED_TOKEN: &str = "threads-ignored-replies-ignored-token";
const READER_TOKEN: &str = "threads-ignored-replies-reader-token";
const VISIBLE_TOKEN: &str = "threads-ignored-replies-visible-token";

#[test]
fn threads_retain_roots_when_every_reply_is_ignored() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_test_database("threads-ignored-replies")
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("bundle_edit_relations=true");

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

	let reader_id = register(services, "threadsreader", READER_TOKEN).await?;
	let ignored_id = register(services, "threadsignored", IGNORED_TOKEN).await?;

	let visible_id = register(services, "threadsvisible", VISIBLE_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let ignored = Client { services, base, token: IGNORED_TOKEN };
	let visible = Client { services, base, token: VISIBLE_TOKEN };
	let room = reader
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	ignored.join(&room).await?;
	visible.join(&room).await?;

	let root_visible = visible
		.message(&room, "root-visible", "visible root")
		.await?;

	ignored
		.reply(&room, "reply-visible", &root_visible)
		.await?;

	visible
		.edit(&room, "edit-visible", &root_visible)
		.await?;

	let root_reader = reader
		.message(&room, "root-reader", "reader root")
		.await?;

	ignored
		.reply(&room, "reply-reader", &root_reader)
		.await?;

	let root_mixed = visible
		.message(&room, "root-mixed", "mixed root")
		.await?;

	ignored
		.reply(&room, "reply-mixed-ignored", &root_mixed)
		.await?;

	visible
		.reply(&room, "reply-mixed-visible", &root_mixed)
		.await?;

	let root_ignored = ignored
		.message(&room, "root-ignored", "ignored root")
		.await?;

	visible
		.reply(&room, "reply-ignored-root", &root_ignored)
		.await?;

	reader
		.set_ignored(&reader_id, &ignored_id)
		.await?;

	let first = reader.threads(&room, 2, None).await?;
	let token = first
		.get("next_batch")
		.and_then(Value::as_str)
		.ok_or_else(|| err!("first thread page omitted its continuation token"))?;

	let second = reader.threads(&room, 2, Some(token)).await?;

	second
		.get("next_batch")
		.is_none()
		.into_option()
		.ok_or_else(|| err!("final thread page retained a continuation token"))?;

	let first_chunk = chunk(&first)?;
	let second_chunk = chunk(&second)?;

	if first_chunk.len() != 2 || second_chunk.len() != 2 {
		return Err!("thread pagination did not retain all four roots");
	}

	let roots: Vec<&str> = first_chunk
		.iter()
		.chain(second_chunk)
		.map(|event| {
			event["event_id"]
				.as_str()
				.expect("event id should be a string")
		})
		.collect();

	assert_eq!(
		roots,
		[
			root_ignored.as_str(),
			root_mixed.as_str(),
			root_reader.as_str(),
			root_visible.as_str(),
		],
		"thread roots lost their activity order",
	);

	let pages = [&first, &second];
	let visible_root = event(&pages, &root_visible)?;
	let reader_root = event(&pages, &root_reader)?;
	let mixed_root = event(&pages, &root_mixed)?;
	let ignored_root = event(&pages, &root_ignored)?;

	assert_eq!(visible_root["content"]["body"], "visible root");
	assert_eq!(reader_root["content"]["body"], "reader root");
	assert!(thread_summary(visible_root).is_none());
	assert!(thread_summary(reader_root).is_none());
	assert!(
		visible_root
			.pointer("/unsigned/m.relations/m.replace")
			.is_some()
	);

	let mixed_summary =
		thread_summary(mixed_root).ok_or_else(|| err!("mixed thread omitted its summary"))?;

	assert_eq!(mixed_summary["count"], 1);
	assert_eq!(mixed_summary["latest_event"]["sender"], visible_id.as_str());

	let ignored_summary = thread_summary(ignored_root)
		.ok_or_else(|| err!("ignored root omitted its visible summary"))?;

	assert_eq!(ignored_root["sender"], ignored_id.as_str());
	assert_eq!(ignored_root["content"], json!({}));
	assert_eq!(ignored_summary["count"], 1);
	assert_eq!(ignored_summary["latest_event"]["sender"], visible_id.as_str());

	Ok(())
}

fn chunk(response: &Value) -> Result<&[Value]> {
	response["chunk"]
		.as_array()
		.map(Vec::as_slice)
		.ok_or_else(|| err!("threads response omitted its chunk"))
}

fn event<'a>(pages: &'a [&Value], event_id: &EventId) -> Result<&'a Value> {
	pages
		.iter()
		.filter_map(|page| page["chunk"].as_array())
		.flatten()
		.find(|event| event["event_id"] == event_id.as_str())
		.ok_or_else(|| err!("threads response omitted {event_id}"))
}

fn thread_summary(event: &Value) -> Option<&Value> {
	event.pointer("/unsigned/m.relations/m.thread")
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

#[implement(Client, params = "<'_>")]
async fn message(&self, room_id: &RoomId, txn: &str, body: &str) -> Result<OwnedEventId> {
	self.send(room_id, txn, &json!({ "msgtype": "m.text", "body": body }))
		.await
}

#[implement(Client, params = "<'_>")]
async fn reply(&self, room_id: &RoomId, txn: &str, root: &EventId) -> Result<OwnedEventId> {
	let content = json!({
		"msgtype": "m.text",
		"body": "thread reply",
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
async fn edit(&self, room_id: &RoomId, txn: &str, root: &EventId) -> Result<OwnedEventId> {
	let content = json!({
		"msgtype": "m.text",
		"body": "* edited visible root",
		"m.new_content": { "msgtype": "m.text", "body": "edited visible root" },
		"m.relates_to": { "rel_type": "m.replace", "event_id": root },
	});

	self.send(room_id, txn, &content).await
}

#[implement(Client, params = "<'_>")]
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

	field(&response, "event_id")?
		.try_into()
		.map_err(Into::into)
}

#[implement(Client, params = "<'_>")]
async fn set_ignored(&self, user_id: &UserId, ignored: &UserId) -> Result {
	let path = format!("user/{user_id}/account_data/m.ignored_user_list");

	self.services
		.client
		.clients
		.default
		.put(self.url(&path))
		.bearer_auth(self.token)
		.json(&json!({ "ignored_users": { ignored.as_str(): {} } }))
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

#[implement(Client, params = "<'_>")]
async fn threads(&self, room_id: &RoomId, limit: usize, from: Option<&str>) -> Result<Value> {
	let url = format!("{}/_matrix/client/v1/rooms/{room_id}/threads", self.base);
	let request = self
		.services
		.client
		.clients
		.default
		.get(url)
		.bearer_auth(self.token)
		.query(&[("limit", limit)]);

	let request = match from {
		| None => request,
		| Some(from) => request.query(&[("from", from)]),
	};

	let response = request
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(response)
}
