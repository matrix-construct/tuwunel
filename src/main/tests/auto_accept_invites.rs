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

const ACCEPT_DEADLINE: Duration = Duration::from_secs(10);

const INVITER_TOKEN: &str = "auto-accept-invites-inviter-token";
const INVITEE_TOKEN: &str = "auto-accept-invites-invitee-token";

/// Whether a created room's invitation is flagged a direct chat.
///
/// The flag reaches the invite's membership content only through room
/// creation, since the invite endpoint has no field carrying it.
#[derive(Clone, Copy)]
enum Chat {
	Direct,
	Plain,
}

/// Drives `auto_accept_invites` end to end over the client API.
///
/// The server is configured to accept direct invites only, so one invitation
/// must be joined on the invitee's behalf and recorded in their `m.direct`,
/// while an otherwise identical invitation to a plain room must be left for
/// them to answer.
#[test]
fn direct_invites_are_accepted_and_others_are_not() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path = PathBuf::from(root).join(format!("tuwunel-auto-accept-{}", process_id()));

	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.option.extend([
		format!("database_path={db_path:?}"),
		"address=[\"127.0.0.1\"]".to_owned(),
		format!("port={port}"),
		"listening=true".to_owned(),
		"auto_accept_invites=true".to_owned(),
		"auto_accept_invites_direct_only=true".to_owned(),
	]);

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

	let inviter_id = register(services, "autoacceptinviter", INVITER_TOKEN).await?;
	let invitee_id = register(services, "autoacceptinvitee", INVITEE_TOKEN).await?;

	let inviter = Client { services, base, token: INVITER_TOKEN };
	let invitee = Client { services, base, token: INVITEE_TOKEN };

	let plain = inviter
		.create_room(&invite_body(&invitee_id, Chat::Plain))
		.await?;

	let direct = inviter
		.create_room(&invite_body(&invitee_id, Chat::Direct))
		.await?;

	joined(services, &invitee_id, &direct)
		.await
		.ok_or_else(|| err!("the direct invitation was never accepted"))?;

	invitee
		.marks_direct(&invitee_id, &inviter_id, &direct)
		.await?
		.ok_or_else(|| err!("the accepted room is missing from the invitee's m.direct"))?;

	services
		.account_data
		.is_direct(&invitee_id, &direct)
		.await
		.ok_or_else(|| err!("the server does not read the accepted room back as direct"))?;

	joined(services, &invitee_id, &plain)
		.await
		.is_false()
		.ok_or_else(|| err!("a plain room invitation was accepted under the direct-only policy"))
}

/// A `createRoom` body inviting `invitee` into a private room.
///
/// Both rooms the test creates differ only in the direct-chat flag, so the
/// rest of the body is written once.
fn invite_body(invitee: &UserId, chat: Chat) -> Value {
	json!({
		"preset": "private_chat",
		"invite": [invitee],
		"is_direct": matches!(chat, Chat::Direct),
	})
}

/// Whether the user's `m.direct` names the room under `counterparty`.
#[implement(Client, params = "<'_>")]
async fn marks_direct(
	&self,
	user_id: &UserId,
	counterparty: &UserId,
	room_id: &RoomId,
) -> Result<bool> {
	let path = format!("user/{user_id}/account_data/m.direct");

	let response: Value = self
		.services
		.client
		.clients
		.default
		.get(self.url(&path))
		.bearer_auth(self.token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	let named = response
		.get(counterparty.as_str())
		.and_then(Value::as_array)
		.is_some_and(|rooms| {
			rooms
				.iter()
				.filter_map(Value::as_str)
				.any(|room| room.eq(room_id.as_str()))
		});

	Ok(named)
}

/// Whether the user reaches joined membership before the deadline.
///
/// Acceptance trails the invite, so membership is polled rather than sampled
/// once.
async fn joined(services: &Services, user_id: &UserId, room_id: &RoomId) -> bool {
	poll_until(ACCEPT_DEADLINE, async || {
		services
			.state_cache
			.is_joined(user_id, room_id)
			.await
	})
	.await
}
