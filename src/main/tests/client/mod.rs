//! Client-API harness shared by the server-booting tests that opt into it.
//!
//! Each such test is its own binary, so everything here would otherwise be
//! copied once per binary. Every item is used by each binary declaring the
//! module, which is what keeps it free of dead code in any one.

use std::time::Duration;

use serde_json::Value;
use tokio::time::{sleep, timeout};
use tuwunel_core::{
	Result, err, implement,
	ruma::{OwnedRoomId, OwnedUserId, UserId},
	utils::BoolExt,
};
use tuwunel_service::{Services, users::Register};

/// Password every harness account registers with.
///
/// No test authenticates through it, since `register` installs the device
/// token each request then carries.
const PASSWORD: &str = "tuwunel-test-harness-password";

const POLL_INTERVAL: Duration = Duration::from_millis(20);

const READY_DEADLINE: Duration = Duration::from_secs(10);

/// One user's authenticated view of the client API.
///
/// The running services own the HTTP client requests go out on, the base
/// carries the ephemeral port the test bound, and the token names the user.
pub(crate) struct Client<'a> {
	pub(crate) services: &'a Services,
	pub(crate) base: &'a str,
	pub(crate) token: &'a str,
}

/// Create a room from a `createRoom` body and return its id.
///
/// The body stays the caller's, since what makes a room interesting differs
/// per test; only the request and the id it answers with are shared.
#[implement(Client, params = "<'_>")]
pub(crate) async fn create_room(&self, body: &Value) -> Result<OwnedRoomId> {
	let response: Value = self
		.services
		.client
		.clients
		.default
		.post(self.url("createRoom"))
		.bearer_auth(self.token)
		.json(body)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(field(&response, "room_id")?.try_into()?)
}

/// The versioned client-API URL for one endpoint path.
///
/// Every request in the harness is addressed through here, so the base and
/// the version prefix are stated once.
#[implement(Client, params = "<'_>")]
pub(crate) fn url(&self, path: &str) -> String {
	format!("{}/_matrix/client/v3/{path}", self.base)
}

/// Wait for the listener to answer, which the boot does not itself await.
///
/// A request issued before then fails to connect, so the probe retries until
/// the versions endpoint answers or the deadline passes.
pub(crate) async fn wait_until_ready(services: &Services, base: &str) -> Result {
	let url = format!("{base}/_matrix/client/versions");

	let ready = poll_until(READY_DEADLINE, async || {
		services
			.client
			.clients
			.default
			.get(&url)
			.send()
			.await
			.is_ok()
	})
	.await;

	ready
		.into_option()
		.ok_or_else(|| err!("server listener did not become ready"))
}

/// Whether the condition holds before the deadline.
///
/// A server-side effect trails the response that triggers it, so neither
/// outcome reads off a single sample: a positive case needs the poll, and a
/// negative case is only proven by the whole deadline elapsing.
// AsyncFn cannot express a Send bound on its call future without an unstable
// associated type, and every caller drives this on one runtime thread.
#[expect(clippy::future_not_send)]
pub(crate) async fn poll_until<Condition>(deadline: Duration, condition: Condition) -> bool
where
	Condition: AsyncFn() -> bool,
{
	timeout(deadline, async {
		while !condition().await {
			sleep(POLL_INTERVAL).await;
		}
	})
	.await
	.is_ok()
}

/// Register a local user and give it a device holding `token`.
///
/// The device is created directly rather than through the client API so the
/// token is known up front and every later request can carry it.
pub(crate) async fn register(
	services: &Services,
	localpart: &str,
	token: &str,
) -> Result<OwnedUserId> {
	let user_id = UserId::parse_with_server_name(localpart, services.globals.server_name())?;

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some(PASSWORD),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	Ok(user_id)
}

/// Read a required string field out of a response body.
///
/// A response missing the field is a test failure rather than a case to
/// handle, so the error names it and the caller propagates.
pub(crate) fn field<'a>(response: &'a Value, name: &str) -> Result<&'a str> {
	response
		.get(name)
		.and_then(Value::as_str)
		.ok_or_else(|| err!("response omitted {name}"))
}
