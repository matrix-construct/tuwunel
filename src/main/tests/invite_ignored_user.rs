#![cfg(test)]

use std::{
	env::var, fs::remove_dir_all, net::TcpListener, path::PathBuf, process::id as process_id,
	time::Duration,
};

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::client::{Client, poll_until, register, wait_until_ready};

mod client;

const INVITE_DEADLINE: Duration = Duration::from_secs(10);

const INVITER_TOKEN: &str = "invite-ignored-user-inviter-token";
const INVITEE_TOKEN: &str = "invite-ignored-user-invitee-token";

/// Drives an invite from a sender the recipient ignores.
///
/// The ignore list decides what sync serves, never what the server stores, so
/// the row has to be written and withheld rather than dropped. Dropping it
/// leaves room state and every federating server naming an invite the
/// recipient can neither see nor recover.
#[test]
fn an_ignored_invite_is_stored_and_surfaces_on_un_ignore() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path = PathBuf::from(root).join(format!("tuwunel-invite-ignored-{}", process_id()));

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

	let inviter_id = register(services, "ignoredinviter", INVITER_TOKEN).await?;
	let invitee_id = register(services, "ignoredinvitee", INVITEE_TOKEN).await?;

	let inviter = Client { services, base, token: INVITER_TOKEN };
	let invitee = Client { services, base, token: INVITEE_TOKEN };

	invitee
		.set_ignored(&invitee_id, Some(&inviter_id))
		.await?;

	let body = json!({ "preset": "private_chat", "invite": [&invitee_id] });
	let room = inviter.create_room(&body).await?;

	invited(services, &invitee_id, &room)
		.await
		.ok_or_else(|| err!("the invite was dropped instead of stored"))?;

	invitee
		.sync_names_invite(&room)
		.await?
		.is_false()
		.ok_or_else(|| err!("sync served an invite from an ignored sender"))?;

	invitee.set_ignored(&invitee_id, None).await?;

	invitee
		.sync_names_invite(&room)
		.await?
		.ok_or_else(|| err!("the pending invite did not surface after un-ignoring"))
}

/// Point the user's `m.ignored_user_list` at `ignored`, or empty it.
///
/// The list is written over the client API rather than the service so the test
/// exercises the same account-data path a client takes.
#[implement(Client, params = "<'_>")]
async fn set_ignored(&self, user_id: &UserId, ignored: Option<&UserId>) -> Result {
	let path = format!("user/{user_id}/account_data/m.ignored_user_list");
	let ignored_users = ignored.map_or_else(|| json!({}), |user| json!({ user.as_str(): {} }));

	self.services
		.client
		.clients
		.default
		.put(self.url(&path))
		.bearer_auth(self.token)
		.json(&json!({ "ignored_users": ignored_users }))
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

/// Whether an initial sync names the room among the invited rooms.
///
/// The sync is unfiltered and takes no timeout, so the answer is the whole
/// current invite set rather than a delta the test would have to sequence.
#[implement(Client, params = "<'_>")]
async fn sync_names_invite(&self, room_id: &RoomId) -> Result<bool> {
	let response: Value = self
		.services
		.client
		.clients
		.default
		.get(self.url("sync?timeout=0"))
		.bearer_auth(self.token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	let named = response
		.pointer("/rooms/invite")
		.and_then(Value::as_object)
		.is_some_and(|invited| invited.contains_key(room_id.as_str()));

	Ok(named)
}

/// Whether the invite row lands before the deadline.
///
/// The membership write trails the createRoom response that triggers it, so
/// the row is polled rather than sampled once.
async fn invited(services: &Services, user_id: &UserId, room_id: &RoomId) -> bool {
	poll_until(INVITE_DEADLINE, async || {
		services
			.state_cache
			.is_invited(user_id, room_id)
			.await
	})
	.await
}
