#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Result, ruma::RoomId, utils::BoolExt};
use tuwunel_service::Services;

use self::{
	account::account,
	client::{field, wait_until_ready},
	device_lists::{device_list_changed, e2ee_device_lists},
	require::require,
};

mod client;
mod sliding_sync;

#[path = "client/account.rs"]
mod account;

#[expect(dead_code)] // The paired read for both surfaces is unused here.
#[path = "client/device_lists.rs"]
mod device_lists;

#[expect(dead_code)] // Only the sliding sync request is used from the add-on.
#[path = "client/e2ee.rs"]
mod e2ee;

#[path = "client/put.rs"]
mod put;

#[path = "client/require.rs"]
mod require;

#[path = "client/state.rs"]
mod state;

#[path = "client/topic.rs"]
mod topic;

#[expect(dead_code)] // Only the self-membership action is used from the add-on.
#[path = "client/membership.rs"]
mod membership;

const READER_TOKEN: &str = "sync-v5-plaintext-device-lists-reader-token";
const SENDER_TOKEN: &str = "sync-v5-plaintext-device-lists-sender-token";

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
		.with_loopback_listener(port)
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

	let (_, reader) = account(services, base, "plaintextreader", READER_TOKEN).await?;
	let (sender_id, sender) = account(services, base, "plaintextsender", SENDER_TOKEN).await?;

	// Asking for the topic proves a round visited the room.
	let required_state = json!([["m.room.topic", ""]]);
	let room = sender
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	reader.act(&room, "join").await?;

	let opening = reader.sync_e2ee(None, &required_state).await?;
	let pos = next_pos(&opening)?;

	sender.set_topic(&room, "still plaintext").await?;

	let retitled = reader
		.sync_e2ee(Some(pos), &required_state)
		.await?;

	let pos = next_pos(&retitled)?;
	let delivered = state_delivered(&retitled, &room, "m.room.topic");
	let listed = device_list_changed(e2ee_device_lists(&retitled), &sender_id);

	require(delivered, "the retitled round did not carry the topic change")?;
	require(
		listed.is_false(),
		"a plaintext state change burst the members into the device lists",
	)?;

	let encryption = json!({ "algorithm": "m.megolm.v1.aes-sha2" });

	sender
		.put_state(&room, "m.room.encryption", "", &encryption)
		.await?;

	let encrypted = reader
		.sync_e2ee(Some(pos), &required_state)
		.await?;

	let listed = device_list_changed(e2ee_device_lists(&encrypted), &sender_id);

	require(listed, "turning on encryption withheld the members from the device lists")
}

fn next_pos(response: &Value) -> Result<&str> { field(response, "pos") }

fn state_delivered(response: &Value, room_id: &RoomId, event_type: &str) -> bool {
	response["rooms"][room_id.as_str()]["required_state"]
		.as_array()
		.into_iter()
		.flatten()
		.any(|event| event["type"] == event_type)
}
