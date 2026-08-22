#![cfg(test)]

use std::{
	env::var, fs::remove_dir_all, net::TcpListener, path::PathBuf, process::id as process_id,
	time::Duration,
};

use futures::future::join;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::{Services, users::Register};

const INVITER_TOKEN: &str = "auto-accept-invites-inviter-token";
const INVITEE_TOKEN: &str = "auto-accept-invites-invitee-token";

/// One user's authenticated view of the client API.
///
/// The three fields are everything a request needs: the running services own
/// the HTTP client, the base carries the ephemeral port, and the token names
/// the user.
struct Client<'a> {
	services: &'a Services,
	base: &'a str,
	token: &'a str,
}

/// Whether a created room's invitation is flagged a direct chat.
///
/// The flag reaches the invite's membership content only through room
/// creation, since the invite endpoint has no field carrying it.
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
		.create_room(&invitee_id, Chat::Plain)
		.await?;
	let direct = inviter
		.create_room(&invitee_id, Chat::Direct)
		.await?;

	joined(services, &invitee_id, &direct)
		.await
		.ok_or_else(|| err!("the direct invitation was never accepted"))?;

	invitee
		.marks_direct(&invitee_id, &inviter_id, &direct)
		.await?
		.ok_or_else(|| err!("the accepted room is missing from the invitee's m.direct"))?;

	joined(services, &invitee_id, &plain)
		.await
		.is_false()
		.ok_or_else(|| err!("a plain room invitation was accepted under the direct-only policy"))
}

/// Create a room inviting `invitee`, flagged a direct chat or not.
#[implement(Client, params = "<'_>")]
async fn create_room(&self, invitee: &UserId, chat: Chat) -> Result<OwnedRoomId> {
	let body = json!({
		"preset": "private_chat",
		"invite": [invitee],
		"is_direct": matches!(chat, Chat::Direct),
	});

	let response: Value = self
		.services
		.client
		.clients
		.default
		.post(self.url("createRoom"))
		.bearer_auth(self.token)
		.json(&body)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	let room_id = response
		.get("room_id")
		.and_then(Value::as_str)
		.ok_or_else(|| err!("response omitted room_id"))?;

	Ok(room_id.try_into()?)
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

#[implement(Client, params = "<'_>")]
fn url(&self, path: &str) -> String { format!("{}/_matrix/client/v3/{path}", self.base) }

/// Wait for the listener to answer, which the boot does not itself await.
async fn wait_until_ready(services: &Services, base: &str) -> Result {
	let url = format!("{base}/_matrix/client/versions");

	timeout(Duration::from_secs(10), async {
		while services
			.client
			.clients
			.default
			.get(&url)
			.send()
			.await
			.is_err()
		{
			sleep(Duration::from_millis(20)).await;
		}
	})
	.await
	.map_err(|_| err!("server listener did not become ready"))
}

/// Register a local user and give it a device holding `token`.
///
/// The device is created directly rather than through the client API so the
/// token is known up front and every later request can carry it.
async fn register(services: &Services, localpart: &str, token: &str) -> Result<OwnedUserId> {
	let user_id = UserId::parse_with_server_name(localpart, services.globals.server_name())?;

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("auto-accept-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	Ok(user_id)
}

/// Whether the user reaches joined membership before the deadline.
///
/// Acceptance trails the invite, so neither outcome reads off a single
/// sample: the positive case needs the poll, and the negative case is only
/// proven by the whole deadline elapsing.
async fn joined(services: &Services, user_id: &UserId, room_id: &RoomId) -> bool {
	timeout(Duration::from_secs(10), async {
		while !services
			.state_cache
			.is_joined(user_id, room_id)
			.await
		{
			sleep(Duration::from_millis(20)).await;
		}
	})
	.await
	.is_ok()
}
