#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{OwnedRoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;
mod sliding_sync;

#[path = "client/get.rs"]
mod get;

#[expect(dead_code)] // Only the invite and self-membership actions are used from the add-on.
#[path = "client/membership.rs"]
mod membership;

const READER_TOKEN: &str = "sync-device-lists-joiners-reader-token";
const JOINER_TOKEN: &str = "sync-device-lists-joiners-joiner-token";

/// How long a resumed sliding sync polls for, in milliseconds.
///
/// The resumed round follows joins already written, so the poll answers on
/// its first pass and the budget only bounds a round that found nothing.
const POLL_TIMEOUT: u64 = 1_500;

/// Drives an invitee joining two encrypted rooms between two of a reader's syncs.
///
/// Once both joins land, each room's join leaves the reader sharing the other
/// encrypted room with the joiner, and that must not hide the join. Both
/// `/sync` and the sliding sync e2ee extension list the joiner in
/// `device_lists.changed`, so the reader's client fetches any device the
/// joiner made while only invited.
#[test]
fn joiners_sharing_another_encrypted_room_are_device_list_changes() -> Result {
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

	register(services, "joinersreader", READER_TOKEN).await?;
	let joiner_id = register(services, "joinersjoiner", JOINER_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let joiner = Client { services, base, token: JOINER_TOKEN };
	let first = reader.create_encrypted_room().await?;
	let second = reader.create_encrypted_room().await?;

	reader
		.act_on(&first, "invite", &joiner_id)
		.await?;

	reader
		.act_on(&second, "invite", &joiner_id)
		.await?;

	let synced = reader.get("sync?timeout=0").await?;
	let since = field(&synced, "next_batch")?;
	let opened = reader.sync_e2ee(None).await?;
	let pos = field(&opened, "pos")?;

	joiner.act(&first, "join").await?;
	joiner.act(&second, "join").await?;

	let joined = reader
		.get(&format!("sync?timeout=0&since={since}"))
		.await?;

	require_changed(&joined["device_lists"], &joiner_id, "/sync")?;

	let joined = reader.sync_e2ee(Some(pos)).await?;

	require_changed(&joined["extensions"]["e2ee"]["device_lists"], &joiner_id, "sliding sync")
}

/// Create an invite-only room with encryption in its initial state.
///
/// The `private_chat` preset is what makes the room invite-only, so the
/// joiner needs an invite before it can join.
#[implement(Client, params = "<'_>")]
async fn create_encrypted_room(&self) -> Result<OwnedRoomId> {
	let encryption = json!({
		"type": "m.room.encryption",
		"state_key": "",
		"content": { "algorithm": "m.megolm.v1.aes-sha2" },
	});

	self.create_room(&json!({ "preset": "private_chat", "initial_state": [encryption] }))
		.await
}

/// One sliding sync carrying the e2ee extension, optionally resuming a pos.
///
/// The single list keeps both rooms in the connection's window; the extension
/// is what the case reads.
#[implement(Client, params = "<'_>")]
async fn sync_e2ee(&self, pos: Option<&str>) -> Result<Value> {
	let body = json!({
		"lists": {
			"all": {
				"ranges": [[0, 9]],
				"required_state": [],
				"timeline_limit": 0,
			},
		},
		"extensions": {
			"e2ee": { "enabled": true },
		},
	});

	self.sliding_sync(pos, POLL_TIMEOUT, &body).await
}

/// Require the joiner in one sync surface's `device_lists.changed`.
///
/// Both sync APIs carry the same list under different roots, so the caller
/// hands in the `device_lists` object and names the surface for the failure.
fn require_changed(device_lists: &Value, user_id: &UserId, surface: &str) -> Result {
	device_lists["changed"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(Value::as_str)
		.any(|user| user == user_id.as_str())
		.into_option()
		.ok_or_else(|| err!("{surface} withheld a joiner who shares another encrypted room"))
}
