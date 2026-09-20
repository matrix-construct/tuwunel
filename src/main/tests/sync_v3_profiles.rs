#![cfg(test)]

use std::{borrow::Cow, net::TcpListener};

use futures::{TryStreamExt, future::join};
use reqwest::{Response, StatusCode};
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
/// none, and one that names a field receives that field alone. Non-lazy initial
/// sync carries current joined peers independently of membership event filters
/// and discovery history. Later rounds carry changes and clearing nulls while
/// enforcing current visibility and failing unreadable updates without a token.
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

	// Simulate profiles stored before discovery logging was introduced.
	services.db["profilechangeid_userid"]
		.for_clear()
		.map_ok(|_| ())
		.try_collect::<()>()
		.await?;

	let room_id = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	peer.post(&format!("rooms/{room_id}/join"), &json!({}))
		.await?;

	filtered_bases(&owner, &owner_id, &peer_id, room_id.as_str()).await?;
	filtered_subjects(&owner, &peer_id, room_id.as_str()).await?;

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
	})?;

	read_failures(&owner, &peer_id, room_id.as_str()).await?;
	membership_failures(&owner, &owner_id, &peer_id, room_id.as_str()).await?;
	departed_profile(&owner, &peer, &peer_id, room_id.as_str()).await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn filtered_bases(
	owner: &Client<'_>,
	owner_id: &UserId,
	peer_id: &UserId,
	room_id: &str,
) -> Result {
	let absent = "org.example.absent";

	for room in [
		json!({ "not_rooms": [room_id] }),
		json!({
			"state": { "not_types": ["m.room.member"] },
			"timeline": { "not_types": ["m.room.member"] },
		}),
	] {
		let filter = json!({ PROFILE_FIELDS: { "ids": [STATUS, absent] }, "room": room });
		let response: Value = owner
			.sync_response(Some(&filter.to_string()), None)
			.await?
			.error_for_status()?
			.json()
			.await?;

		assert_eq!(update(&response, peer_id)[STATUS]["text"], "away");
		assert_eq!(update(&response, owner_id)[STATUS]["text"], "busy");
		assert_eq!(update(&response, peer_id).get(absent), Some(&Value::Null));
		assert_eq!(update(&response, owner_id).get(absent), Some(&Value::Null));
	}

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn filtered_subjects(owner: &Client<'_>, peer_id: &UserId, room_id: &str) -> Result {
	let lazy = json!({
		PROFILE_FIELDS: { "ids": [STATUS] },
		"room": {
			"state": { "not_types": ["m.room.member"], "lazy_load_members": true },
			"timeline": { "not_types": ["m.room.member"], "lazy_load_members": true },
		},
	})
	.to_string();

	let response: Value = owner
		.sync_response(Some(&lazy), None)
		.await?
		.error_for_status()?
		.json()
		.await?;

	assert!(update(&response, peer_id).is_null(), "lazy sync refilled an unwitnessed peer");

	let opening = owner.sync(Some(&[]), None).await?;
	let since = field(&opening, "next_batch")?;
	let filter = json!({
		PROFILE_FIELDS: { "ids": [STATUS] },
		"room": { "not_rooms": [room_id] },
	})
	.to_string();

	for since in ["0", since] {
		let response: Value = owner
			.sync_response(Some(&filter), Some(since))
			.await?
			.error_for_status()?
			.json()
			.await?;

		assert!(update(&response, peer_id).is_null(), "supplied since refilled a peer base");
	}

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn read_failures(owner: &Client<'_>, peer_id: &UserId, room_id: &str) -> Result {
	let opening = owner.sync(Some(&[STATUS]), None).await?;
	let since = field(&opening, "next_batch")?;
	let status = json!({ "text": "readable", "emoji": "" });
	let encoded = status.to_string();

	set_status(owner.services, peer_id, Some(status)).await?;

	let count = owner.services.globals.current_count();
	let profiles = &owner.services.db["useridprofilekey_value"];

	profiles.put_raw((peer_id, STATUS), b"not-json");
	fails_without_token(owner, since).await?;
	profiles.put_raw((peer_id, STATUS), encoded.as_bytes());

	let repaired = owner.sync(Some(&[STATUS]), Some(since)).await?;

	assert_eq!(update(&repaired, peer_id)[STATUS]["text"], "readable");

	let changes = &owner.services.db["profilechangeid_userid"];
	let key = (room_id, count, STATUS);
	let recorded = changes.qry(&key).await?;

	assert_eq!(recorded.as_ref(), peer_id.as_bytes());

	changes.put_raw(key, b"not-a-user-id");
	fails_without_token(owner, since).await?;
	changes.put_raw(key, peer_id.as_bytes());

	let repaired = owner.sync(Some(&[STATUS]), Some(since)).await?;

	assert_eq!(update(&repaired, peer_id)[STATUS]["text"], "readable");

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn fails_without_token(owner: &Client<'_>, since: &str) -> Result {
	let filter = json!({ PROFILE_FIELDS: { "ids": [STATUS] } }).to_string();

	fails_with_filter(owner, &filter, Some(since)).await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn fails_with_filter(owner: &Client<'_>, filter: &str, since: Option<&str>) -> Result {
	let response = owner.sync_response(Some(filter), since).await?;

	assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

	let body: Value = response.json().await?;

	assert!(body.get("next_batch").is_none());

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn membership_failures(
	owner: &Client<'_>,
	owner_id: &UserId,
	peer_id: &UserId,
	room_id: &str,
) -> Result {
	// Exclude room payloads so only profile collection encounters these rows.
	let filter = json!({
		PROFILE_FIELDS: { "ids": [STATUS] },
		"room": { "not_rooms": [room_id] },
	})
	.to_string();

	let members = &owner.services.db["roomuserid_joined"];
	let invalid = (room_id, "not-a-user-id");

	members.put_raw(invalid, 1_u64.to_be_bytes());
	fails_with_filter(owner, &filter, None).await?;
	members.del(invalid);

	let repaired: Value = owner
		.sync_response(Some(&filter), None)
		.await?
		.error_for_status()?
		.json()
		.await?;

	assert_eq!(update(&repaired, peer_id)[STATUS]["text"], "readable");

	let key = (room_id, owner_id);
	let saved = members.qry(&key).await?;

	members.put_raw(key, b"invalid-count");
	fails_with_filter(owner, &filter, None).await?;
	members.put_raw(key, saved.as_ref());

	let repaired: Value = owner
		.sync_response(Some(&filter), None)
		.await?
		.error_for_status()?
		.json()
		.await?;

	assert_eq!(update(&repaired, peer_id)[STATUS]["text"], "readable");

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn departed_profile(
	owner: &Client<'_>,
	peer: &Client<'_>,
	peer_id: &UserId,
	room_id: &str,
) -> Result {
	let shared = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	peer.post(&format!("rooms/{shared}/join"), &json!({}))
		.await?;

	let opening = owner.sync(Some(&[STATUS]), None).await?;
	let since = field(&opening, "next_batch")?;

	set_status(owner.services, peer_id, Some(json!({ "text": "shared", "emoji": "" }))).await?;

	peer.post(&format!("rooms/{room_id}/leave"), &json!({}))
		.await?;

	set_status(owner.services, peer_id, Some(json!({ "text": "still shared", "emoji": "" })))
		.await?;

	for since in [Some(since), None] {
		let response = owner.sync(Some(&[STATUS]), since).await?;

		assert_eq!(update(&response, peer_id)[STATUS]["text"], "still shared");
	}

	peer.post(&format!("rooms/{shared}/leave"), &json!({}))
		.await?;

	set_status(owner.services, peer_id, Some(json!({ "text": "private", "emoji": "" }))).await?;

	for since in [Some(since), None] {
		let response = owner.sync(Some(&[STATUS]), since).await?;

		assert!(update(&response, peer_id).is_null(), "a departed peer leaked its profile");
	}

	Ok(())
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
#[tracing::instrument(level = "trace", skip_all)]
async fn sync(&self, fields: Option<&[&str]>, since: Option<&str>) -> Result<Value> {
	let filter = fields.map(|ids| json!({ PROFILE_FIELDS: { "ids": ids } }).to_string());

	self.sync_response(filter.as_deref(), since)
		.await?
		.error_for_status()?
		.json()
		.await
		.map_err(Into::into)
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "trace", skip_all)]
async fn sync_response(&self, filter: Option<&str>, since: Option<&str>) -> Result<Response> {
	let query: Vec<QueryParam<'_>> = filter
		.map(|filter| ("filter", Cow::Borrowed(filter)))
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
