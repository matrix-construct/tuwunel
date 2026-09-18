#![cfg(test)]

use std::{borrow::Cow, net::TcpListener};

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{UserId, profile::ProfileFieldName},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const OWNER_TOKEN: &str = "sync-v3-profiles-owner-access-token";

const PEER_TOKEN: &str = "sync-v3-profiles-peer-access-token";

const STATUS: &str = "org.matrix.msc4426.status";

const USERS: &str = "org.matrix.msc4429.users";

const PROFILE_FIELDS: &str = "org.matrix.msc4429.profile_fields";

/// One query parameter of a sync request, borrowed or built as needed.
type QueryParam<'a> = (&'a str, Cow<'a, str>);

/// How long a resumed sync polls for, in milliseconds.
///
/// Every write this test waits on precedes the round that reads it, so the
/// budget only bounds a round that found nothing.
const POLL_TIMEOUT: u64 = 1_500;

/// Drives the MSC4429 profile updates Element Web reads from legacy sync.
///
/// The block is filtered: a client that asks for no profile fields receives
/// none, and one that names a field receives that field alone. An initial sync
/// carries the current values of the members it was sent, so a status set
/// before the client ever synced still reaches it, and a later round carries
/// the change and the `null` that clears it.
#[test]
fn serves_filtered_profile_updates() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		// A presence ping would fill a round this test expects to find empty.
		.with_option("allow_local_presence=false")
		.with_option("allow_outgoing_presence=false");

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

	let owner_id = register(services, "profileowner", OWNER_TOKEN).await?;
	let peer_id = register(services, "profilepeer", PEER_TOKEN).await?;
	let owner = Client { services, base, token: OWNER_TOKEN };
	let peer = Client { services, base, token: PEER_TOKEN };

	// Written before the peer joins, so the change log holds no row under the
	// room and only the base can carry it to the owner.
	set_status(services, &peer_id, Some(json!({ "text": "away", "emoji": "🌴" }))).await?;
	set_status(services, &owner_id, Some(json!({ "text": "busy", "emoji": "🔴" }))).await?;

	let room_id = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	peer.post(&format!("rooms/{room_id}/join"), &json!({}))
		.await?;

	let unfiltered = owner.sync(None, None).await?;

	BoolExt::ok_or_else(unfiltered.get(USERS).is_none(), || {
		err!("a sync asking for no profile fields still received a users block")
	})?;

	let opening = owner.sync(Some(&[STATUS]), None).await?;
	let since = field(&opening, "next_batch")?;
	let opened = update(&opening, &peer_id);

	// The two users hold different text, so this names whose status arrived.
	BoolExt::ok_or_else(opened[STATUS]["text"] == "away", || {
		err!("the initial sync omitted the status the peer set before it began")
	})?;

	BoolExt::ok_or_else(update(&opening, &owner_id)[STATUS]["text"] == "busy", || {
		err!("the initial sync omitted the syncing user's own status")
	})?;

	BoolExt::ok_or_else(opened.get("displayname").is_none(), || {
		err!("the initial sync carried a field the filter never asked for")
	})?;

	set_status(services, &peer_id, Some(json!({ "text": "back", "emoji": "💻" }))).await?;

	let changed = owner.sync(Some(&[STATUS]), Some(since)).await?;
	let since = field(&changed, "next_batch")?;

	BoolExt::ok_or_else(update(&changed, &peer_id)[STATUS]["text"] == "back", || {
		err!("the resumed sync did not carry the peer's changed status")
	})?;

	set_status(services, &peer_id, None).await?;

	let cleared = owner.sync(Some(&[STATUS]), Some(since)).await?;
	let since = field(&cleared, "next_batch")?;
	let removed = update(&cleared, &peer_id).get(STATUS);

	BoolExt::ok_or_else(removed.is_some_and(Value::is_null), || {
		err!("clearing the status did not reach the client as a removal")
	})?;

	// A real value between the two null rounds, so that the round below cannot be
	// satisfied by this removal reaching the client a second time.
	set_status(services, &peer_id, Some(json!({ "text": "here", "emoji": "👋" }))).await?;

	let restored = owner.sync(Some(&[STATUS]), Some(since)).await?;
	let since = field(&restored, "next_batch")?;

	BoolExt::ok_or_else(update(&restored, &peer_id)[STATUS]["text"] == "here", || {
		err!("setting the status again after a removal did not reach the client")
	})?;

	// Element Web clears by storing a literal null rather than by deleting, which
	// reaches the renderer as a value it read back rather than as an absent row.
	set_status(services, &peer_id, Some(Value::Null)).await?;

	let stored = owner.sync(Some(&[STATUS]), Some(since)).await?;
	let stored = update(&stored, &peer_id).get(STATUS);

	BoolExt::ok_or_else(stored.is_some_and(Value::is_null), || {
		err!("a stored null did not reach the client as a cleared field")
	})
}

/// Sets or clears one profile field, as a profile write would.
///
/// An absent value is the clear path, which is what a client sends to drop a
/// field rather than to store a literal `null`.
async fn set_status(services: &Services, user_id: &UserId, value: Option<Value>) -> Result {
	let field = (ProfileFieldName::from(STATUS), value);

	services
		.profile
		.set_profile_keys(user_id, &[field], None)
		.await
}

/// Syncs once, naming the profile fields the client wants updates for.
///
/// An absent list omits the filter entirely, which is what every client that
/// has not opted in sends. The poll budget rides the token, since only a
/// resumed round can wait on a write the caller has already made.
#[implement(Client, params = "<'_>")]
async fn sync(&self, fields: Option<&[&str]>, since: Option<&str>) -> Result<Value> {
	let filter = fields.map(|ids| json!({ PROFILE_FIELDS: { "ids": ids } }).to_string());
	let query: Vec<QueryParam<'_>> = filter
		.map(|filter| ("filter", Cow::Owned(filter)))
		.into_iter()
		.chain(since.map(|since| ("since", Cow::Borrowed(since))))
		.chain(since.map(|_| ("timeout", Cow::Owned(POLL_TIMEOUT.to_string()))))
		.collect();

	let url = format!("{}/_matrix/client/v3/sync", self.base);

	self.services
		.client
		.clients
		.default
		.get(url)
		.query(&query)
		.bearer_auth(self.token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await
		.map_err(Into::into)
}

/// The profile fields one round reports for a user.
///
/// Indexing answers `null` for an absent user or an absent block as readily as
/// for a field the server cleared, so a caller asserting a removal tests that
/// the field is present as well as null.
fn update<'a>(response: &'a Value, user_id: &UserId) -> &'a Value {
	&response[USERS][user_id.as_str()]["profile_updates"]
}
