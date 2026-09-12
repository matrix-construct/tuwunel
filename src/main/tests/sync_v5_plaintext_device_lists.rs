#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const READER_TOKEN: &str = "sync-v5-plaintext-device-lists-reader-token";
const SENDER_TOKEN: &str = "sync-v5-plaintext-device-lists-sender-token";

/// How long a resumed sync polls for, in milliseconds.
///
/// Every resumed round follows a change already written, so the poll answers
/// on its first pass and the budget only bounds a round that found nothing.
const POLL_TIMEOUT: u64 = 1_500;

/// Drives the sliding-sync e2ee extension over a plaintext room.
///
/// A state change in a plaintext room is not a reason to re-download anyone's
/// keys, so the reader's `device_lists.changed` must stay clear of the other
/// member, while the room turning on encryption must list them.
#[test]
fn plaintext_state_changes_do_not_burst_device_lists() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let driven = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (served, outcome) = join(async_run(&server), driven).await;

		drop(services);
		async_stop(&server).await?;
		served?;

		outcome
	});

	drop(runtime);

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	register(services, "plaintextreader", READER_TOKEN).await?;
	let sender_id = register(services, "plaintextsender", SENDER_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let sender = Client { services, base, token: SENDER_TOKEN };
	let room = sender
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	reader.join(&room).await?;

	let opening = reader.sync_e2ee(None).await?;
	let pos = next_pos(&opening)?;
	let topic = json!({ "topic": "still plaintext" });

	sender
		.put_state(&room, "m.room.topic", &topic)
		.await?;

	let retitled = reader.sync_e2ee(Some(pos)).await?;
	let pos = next_pos(&retitled)?;

	state_delivered(&retitled, &room, "m.room.topic")
		.ok_or_else(|| err!("the retitled round did not carry the topic change"))?;

	device_list_changed(&retitled, &sender_id)
		.is_false()
		.ok_or_else(|| {
			err!("a plaintext state change burst the members into the device lists")
		})?;

	let encryption = json!({ "algorithm": "m.megolm.v1.aes-sha2" });

	sender
		.put_state(&room, "m.room.encryption", &encryption)
		.await?;

	let encrypted = reader.sync_e2ee(Some(pos)).await?;

	device_list_changed(&encrypted, &sender_id).ok_or_else(|| {
		err!("turning on encryption withheld the members from the device lists")
	})?;

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

/// One sliding sync carrying the e2ee extension, optionally resuming a pos.
///
/// The single list keeps the room in the connection's window and asks for the
/// topic, which proves a round visited the room; the extension is what the
/// case reads.
#[implement(Client, params = "<'_>")]
async fn sync_e2ee(&self, pos: Option<&str>) -> Result<Value> {
	let body = json!({
		"lists": {
			"all": {
				"ranges": [[0, 9]],
				"required_state": [["m.room.topic", ""]],
				"timeline_limit": 0,
			},
		},
		"extensions": {
			"e2ee": { "enabled": true },
		},
	});

	let query = pos.map_or_else(String::new, |pos| format!("?pos={pos}&timeout={POLL_TIMEOUT}"));

	let url = format!(
		"{}/_matrix/client/unstable/org.matrix.simplified_msc3575/sync{query}",
		self.base
	);

	self.services
		.client
		.clients
		.default
		.post(url)
		.bearer_auth(self.token)
		.json(&body)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await
		.map_err(Into::into)
}

fn next_pos(response: &Value) -> Result<&str> { field(response, "pos") }

#[implement(Client, params = "<'_>")]
async fn put_state(&self, room_id: &RoomId, event_type: &str, content: &Value) -> Result {
	self.services
		.client
		.clients
		.default
		.put(self.url(&format!("rooms/{room_id}/state/{event_type}")))
		.bearer_auth(self.token)
		.json(content)
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

fn state_delivered(response: &Value, room_id: &RoomId, event_type: &str) -> bool {
	response["rooms"][room_id.as_str()]["required_state"]
		.as_array()
		.into_iter()
		.flatten()
		.any(|event| event["type"] == event_type)
}

fn device_list_changed(response: &Value, user_id: &UserId) -> bool {
	response["extensions"]["e2ee"]["device_lists"]["changed"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(Value::as_str)
		.any(|user| user == user_id.as_str())
}
