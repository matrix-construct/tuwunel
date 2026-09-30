#![cfg(test)]

use std::{net::TcpListener, pin::pin, time::Duration};

use futures::{
	FutureExt,
	future::{Either, join, select},
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::time::sleep;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	ruma::{OwnedUserId, UserId, api::error::ErrorKind},
};
use tuwunel_service::{Services, users::Register};

use self::client::wait_until_ready;

#[expect(
	dead_code,
	reason = "the shared client harness exposes helpers used by sibling integration tests"
)]
mod client;

const PASSWORD: &str = "last-admin-deactivation-password";
const FIRST_TOKEN: &str = "last-admin-deactivation-first-access-token";
const SECOND_TOKEN: &str = "last-admin-deactivation-second-access-token";
const THIRD_TOKEN: &str = "last-admin-deactivation-third-access-token";

/// How long a deactivation must stay waiting on the locked admins room.
const LOCK_WAIT: Duration = Duration::from_millis(500);

/// The last active admin cannot be deactivated, by themselves or by anyone.
///
/// A second active admin lifts the refusal. A deactivated account still joined
/// to the admins room does not count as that second admin, for deactivation or
/// for revocation, and the server user's own deactivation is unaffected. A
/// deactivation also waits on the admins room, so one overtaken there by
/// another admin's deactivation is refused.
#[test]
fn last_admin_cannot_be_deactivated() -> Result {
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
			let outcome = exercise(&services, &base)
				.boxed_local() // size firewall
				.await;
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

	let first = local_user(services, "first-admin", FIRST_TOKEN).await?;
	let second = local_user(services, "second-admin", SECOND_TOKEN).await?;

	services
		.admin
		.make_user_admin(&first)
		.boxed()
		.await?;

	// The only admin is refused on the client route, on the shared deactivation,
	// and on the account primitive the no-leave and OIDC paths call directly.
	self_deactivation_is_forbidden(services, base, &first, FIRST_TOKEN)
		.boxed()
		.await?;
	expect_forbidden(
		services
			.deactivate
			.full_deactivate(&first, false)
			.boxed()
			.await,
		"full deactivation",
	)?;
	expect_forbidden(
		services
			.users
			.deactivate_account(&first)
			.boxed()
			.await,
		"account deactivation",
	)?;
	expect_intact_admin(services, &first).await?;

	// A second active admin lifts the refusal. The no-leave path keeps the
	// deactivated first admin joined to the admins room.
	services
		.admin
		.make_user_admin(&second)
		.boxed()
		.await?;
	services
		.users
		.deactivate_account(&first)
		.boxed()
		.await?;

	if !services.users.is_deactivated(&first).await? {
		return Err!("an admin with another active admin beside them must be deactivatable");
	}

	if !services.admin.user_is_admin(&first).await {
		return Err!(
			"the no-leave path must leave the deactivated admin joined to the admins room"
		);
	}

	// That deactivated member cannot sign in to act, so it is not a second admin.
	self_deactivation_is_forbidden(services, base, &second, SECOND_TOKEN)
		.boxed()
		.await?;
	expect_forbidden(
		services
			.deactivate
			.full_deactivate(&second, false)
			.boxed()
			.await,
		"full deactivation beside a deactivated admin",
	)?;
	expect_intact_admin(services, &second).await?;

	// Nor does it let the admins room's leave guard pass a revocation.
	expect_forbidden(
		services
			.admin
			.revoke_admin(&second)
			.boxed() // size firewall
			.await,
		"revocation beside a deactivated admin",
	)?;

	expect_intact_admin(services, &second).await?;

	let third = local_user(services, "third-admin", THIRD_TOKEN).await?;

	services
		.admin
		.make_user_admin(&third)
		.boxed() // size firewall
		.await?;

	overtaken_deactivation_is_refused(services, &second, &third)
		.boxed() // size firewall
		.await?;

	// The emergency service deactivates the server user when its password is
	// unset, and the server user is never counted as an admin here.
	services
		.users
		.deactivate_account(&services.globals.server_user)
		.boxed()
		.await
}

async fn local_user(services: &Services, localpart: &str, token: &str) -> Result<OwnedUserId> {
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

/// Deactivate an account through the client API, answering the password stage.
///
/// This is the route a client's own "deactivate account" setting calls, so the
/// refusal is asserted as the status and error code a client receives.
async fn self_deactivation_is_forbidden(
	services: &Services,
	base: &str,
	user_id: &UserId,
	token: &str,
) -> Result {
	let url = format!("{base}/_matrix/client/v3/account/deactivate");
	let client = &services.client.clients.default;

	let challenge = client
		.post(&url)
		.bearer_auth(token)
		.json(&json!({}))
		.send()
		.await?;

	if challenge.status() != StatusCode::UNAUTHORIZED {
		return Err!("{user_id}: expected a UIAA challenge, got {}", challenge.status());
	}

	let challenge: Value = challenge.json().await?;
	let session = challenge["session"].clone();

	let response = client
		.post(&url)
		.bearer_auth(token)
		.json(&json!({
			"auth": {
				"type": "m.login.password",
				"identifier": {"type": "m.id.user", "user": user_id.localpart()},
				"password": PASSWORD,
				"session": session,
			},
		}))
		.send()
		.await?;

	let status = response.status();
	let body: Value = response.json().await?;

	if status != StatusCode::FORBIDDEN || body["errcode"] != "M_FORBIDDEN" {
		return Err!("{user_id}: expected M_FORBIDDEN for the last admin, got {status} {body}");
	}

	Ok(())
}

/// Deactivate `target` while the admins room is locked, standing in for
/// `rival`'s deactivation holding the lock first.
///
/// The deactivation must wait for the lock instead of counting admins before
/// taking it. Once the rival has lost its password there, `target` is the last
/// active admin and must be refused.
async fn overtaken_deactivation_is_refused(
	services: &Services,
	target: &UserId,
	rival: &UserId,
) -> Result {
	let admin_lock = services.admin.lock_admin_room().await;
	let deactivation = pin!(services.users.deactivate_account(target));
	let waiting = pin!(sleep(LOCK_WAIT));

	let deactivation = match select(deactivation, waiting).await {
		| Either::Right(((), deactivation)) => deactivation,
		| Either::Left((result, _)) => {
			return Err!("{target}: deactivation did not wait for the admins room: {result:?}");
		},
	};

	// The rival's deactivation, past its own check, clears its password here.
	services.users.set_password(rival, None).await?;
	drop(admin_lock);

	expect_forbidden(deactivation.await, "deactivation overtaken in the admins room")?;
	expect_intact_admin(services, target).await
}

fn expect_forbidden(result: Result, path: &str) -> Result {
	match result {
		| Err(e) if matches!(e.kind(), ErrorKind::Forbidden) => Ok(()),
		| Err(e) => Err!("{path}: unexpected refusal of the last admin: {e}"),
		| Ok(()) => Err!("{path}: the last admin must not be deactivatable"),
	}
}

async fn expect_intact_admin(services: &Services, user_id: &UserId) -> Result {
	if services.users.is_deactivated(user_id).await? {
		return Err!("{user_id}: a refused deactivation must leave the account active");
	}

	if !services.admin.user_is_admin(user_id).await {
		return Err!("{user_id}: a refused deactivation must leave the admin in the admins room");
	}

	Ok(())
}
