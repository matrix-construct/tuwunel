#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{UserId, profile::ProfileFieldName},
	utils::BoolExt,
};
use tuwunel_database::Json;
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

#[expect(
	dead_code,
	reason = "the shared client harness exposes helpers used by sibling integration tests"
)]
mod client;

const TOKEN: &str = "sync-v5-profiles-own-base-test-access-token";

const STATUS: &str = "org.matrix.msc4426.status";

const PROFILES: &str = "org.matrix.msc4262.profiles";

/// An avatar written straight into the profile, bypassing the change log.
///
/// Every field set before the log existed looks like this, and the seed must
/// find it with no log entry to go by.
const PRELOG_AVATAR: &str = "mxc://localhost/prelog-avatar";

/// How long a resumed sync polls for, in milliseconds.
///
/// The status write precedes the resumed round, so the poll answers on its
/// first pass and the budget only bounds a round that found nothing.
const POLL_TIMEOUT: u64 = 1_500;

/// Drives the sliding-sync profiles extension for the syncing user's own
/// profile.
///
/// Element X reads its own avatar and name from this extension alone, so a
/// connection's first response must carry the whole profile, including a
/// field the change log never saw, and a later round must carry only what
/// changed since.
#[test]
fn first_response_seeds_the_whole_own_profile() -> Result {
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

	let user_id = register(services, "profileowner", TOKEN).await?;
	let owner = Client { services, base, token: TOKEN };

	services.db["useridprofilekey_value"].put((&user_id, "avatar_url"), Json(PRELOG_AVATAR));

	let opening = owner.sync_profiles(None).await?;
	let pos = field(&opening, "pos")?;
	let updated = own_update(&opening, &user_id);

	BoolExt::ok_or_else(updated["avatar_url"] == PRELOG_AVATAR, || {
		err!("the opening round omitted the avatar the log never saw")
	})?;

	BoolExt::ok_or_else(updated["displayname"].is_string(), || {
		err!("the opening round omitted the display name")
	})?;

	let status = json!({ "text": "away", "emoji": "🌴" });

	services
		.profile
		.set_profile_keys(&user_id, &[(ProfileFieldName::from(STATUS), Some(status))], None)
		.await?;

	let resumed = owner.sync_profiles(Some(pos)).await?;
	let updated = own_update(&resumed, &user_id);

	BoolExt::ok_or_else(updated[STATUS].is_object(), || {
		err!("the resumed round omitted the status")
	})?;

	BoolExt::ok_or_else(updated.get("avatar_url").is_none(), || {
		err!("the resumed round seeded the profile a second time")
	})?;

	Ok(())
}

/// One sliding sync carrying the profiles extension, optionally resuming a
/// pos.
///
/// The user is in no room, so the extension has only the syncing user's own
/// profile to speak of.
#[implement(Client, params = "<'_>")]
async fn sync_profiles(&self, pos: Option<&str>) -> Result<Value> {
	let body = json!({
		"lists": {},
		"extensions": {
			PROFILES: { "enabled": true },
		},
	});

	let query = pos
		.map(|pos| format!("?pos={pos}&timeout={POLL_TIMEOUT}"))
		.unwrap_or_default();

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

/// The fields the round reports as updated on the syncing user's own profile.
fn own_update<'a>(response: &'a Value, user_id: &UserId) -> &'a Value {
	&response["extensions"][PROFILES]["users"][user_id.as_str()]["updated"]
}
