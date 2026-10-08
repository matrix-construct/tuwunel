#![cfg(test)]

use std::net::TcpListener;

use futures::{TryStreamExt, future::join};
use reqwest::{Response, StatusCode};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	config::check::reload,
	implement,
	ruma::{UserId, profile::ProfileFieldName},
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const OWNER_TOKEN: &str = "sync-v5-profiles-owner-access-token";
const PEER_TOKEN: &str = "sync-v5-profiles-peer-access-token";
const STATUS: &str = "org.matrix.msc4426.status";
const PROFILES: &str = "org.matrix.msc4262.profiles";

#[derive(Clone, Copy)]
enum Selection {
	NonLazy,
	Lazy,
	Timeline,
	Disabled,
	Empty,
}

#[test]
fn serves_visible_profiles_without_acknowledging_failures() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("allow_local_presence=false")
		.with_option("allow_outgoing_presence=false")
		.with_option("max_profile_fields_per_request=2");

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

#[tracing::instrument(level = "trace", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let owner_id = register(services, "slidingowner", OWNER_TOKEN).await?;
	let peer_id = register(services, "slidingpeer", PEER_TOKEN).await?;
	let owner = Client { services, base, token: OWNER_TOKEN };
	let peer = Client { services, base, token: PEER_TOKEN };

	set_status(services, &peer_id, "before joining").await?;
	services.db["profilechangeid_userid"]
		.for_clear()
		.map_ok(|_| ())
		.try_collect::<()>()
		.await?;

	let room = owner
		.create_room(&json!({ "preset": "public_chat", "name": "Profile fixture" }))
		.await?;

	peer.post(&format!("rooms/{room}/join"), &json!({}))
		.await?;

	let opening = owner
		.sync_profiles("base", room.as_str(), None)
		.await?;

	let product = &opening["rooms"][room.as_str()];

	assert!(product.is_object(), "the selected room must be returned");
	assert!(
		product["required_state"]
			.as_array()
			.is_none_or(Vec::is_empty)
	);

	assert!(
		product["timeline"]
			.as_array()
			.is_none_or(Vec::is_empty)
	);

	assert_eq!(update(&opening, &peer_id)[STATUS]["text"], "before joining");
	assert_eq!(
		opening["extensions"][PROFILES]["users"][owner_id.as_str()]["removed"],
		json!([STATUS])
	);

	let lazy: Value = owner
		.sync_selected("lazy", room.as_str(), None, Selection::Lazy)
		.await?
		.error_for_status()?
		.json()
		.await?;

	assert!(update(&lazy, &peer_id).is_null(), "lazy selection refilled every member");

	set_status(services, &peer_id, "historical lazy").await?;

	let historical: Value = owner
		.sync_selected("lazy-history", room.as_str(), None, Selection::Lazy)
		.await?
		.error_for_status()?
		.json()
		.await?;

	assert!(update(&historical, &peer_id).is_null(), "room history bypassed lazy selection");

	active_room(&owner, &peer, &peer_id, room.as_str()).await?;
	read_failures(&owner, &peer_id, room.as_str()).await?;
	membership_failures(&owner, &owner_id, &peer_id, room.as_str()).await?;
	cleared_fields(&owner, &peer_id, room.as_str()).await?;
	departed_profile(&owner, &peer, &peer_id, room.as_str()).await?;
	room_admission(&owner, &peer, &owner_id, &peer_id).await?;
	selector_limits(&owner, &owner_id).await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn selector_limits(owner: &Client<'_>, owner_id: &UserId) -> Result {
	for fields in [json!([STATUS, STATUS, STATUS]), json!(["a".repeat(256)])] {
		let response = owner
			.sync_field_selection("rejected", None, Some(&fields))
			.await?;

		assert_eq!(response.status(), StatusCode::BAD_REQUEST);
	}

	let boundary = json!(["a".repeat(255), STATUS]);

	owner
		.sync_field_selection("boundary", None, Some(&boundary))
		.await?
		.error_for_status()?;

	let duplicate = json!([STATUS, STATUS]);

	owner
		.sync_field_selection("duplicate", None, Some(&duplicate))
		.await?
		.error_for_status()?;

	let selected = json!([STATUS, "displayname"]);
	let opening: Value = owner
		.sync_field_selection("tightened", None, Some(&selected))
		.await?
		.error_for_status()?
		.json()
		.await?;

	let pos = field(&opening, "pos")?;
	let mut config = (*owner.services.server.config.clone()).clone();

	config.max_profile_fields_per_request = 1;
	reload(&owner.services.config, &config)?;
	owner.services.server.config.update(config)?;

	let response = owner
		.sync_field_selection("tightened", Some(pos), None)
		.await?;

	assert_eq!(response.status(), StatusCode::BAD_REQUEST);

	let selected = json!([STATUS]);

	owner
		.sync_field_selection("tightened", Some(pos), Some(&selected))
		.await?
		.error_for_status()?;

	set_status(owner.services, owner_id, "selected").await?;

	let unfiltered: Value = owner
		.sync_field_selection("all-fields", None, None)
		.await?
		.error_for_status()?
		.json()
		.await?;

	assert_eq!(update(&unfiltered, owner_id)[STATUS]["text"], "selected");

	let empty: Value = owner
		.sync_field_selection("no-fields", None, Some(&json!([])))
		.await?
		.error_for_status()?
		.json()
		.await?;

	assert!(update(&empty, owner_id).is_null());

	Ok(())
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "trace", skip_all)]
async fn sync_field_selection(
	&self,
	conn: &str,
	pos: Option<&str>,
	fields: Option<&Value>,
) -> Result<Response> {
	let mut body = json!({
		"conn_id": conn,
		"extensions": { PROFILES: { "enabled": true } },
	});

	if let Some(fields) = fields {
		body["extensions"][PROFILES]["fields"] = fields.clone();
	}

	let query = pos
		.map(|pos| format!("?pos={pos}&timeout=0"))
		.unwrap_or_default();

	let url = format!(
		"{}/_matrix/client/unstable/org.matrix.simplified_msc3575/sync{query}",
		self.base
	);

	self.post_url(&url, &body).await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn active_room(
	owner: &Client<'_>,
	peer: &Client<'_>,
	peer_id: &UserId,
	room: &str,
) -> Result {
	let opening: Value = owner
		.sync_selected("active", room, None, Selection::Timeline)
		.await?
		.error_for_status()?
		.json()
		.await?;

	let pos = field(&opening, "pos")?;
	let body = json!({ "msgtype": "m.text", "body": "ordinary room activity" });
	let path = format!("rooms/{room}/send/m.room.message/profile-activity");

	peer.services
		.client
		.clients
		.default
		.put(peer.url(&path))
		.bearer_auth(peer.token)
		.json(&body)
		.send()
		.await?
		.error_for_status()?;

	let active: Value = owner
		.sync_selected("active", room, Some(pos), Selection::Timeline)
		.await?
		.error_for_status()?
		.json()
		.await?;

	assert_eq!(active["rooms"][room]["timeline"][0]["sender"], peer_id.as_str());
	assert!(update(&active, peer_id).is_null(), "room activity regenerated a profile base");

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn read_failures(owner: &Client<'_>, peer_id: &UserId, room: &str) -> Result {
	let opening = owner
		.sync_profiles("failures", room, None)
		.await?;

	let pos = field(&opening, "pos")?;

	set_status(owner.services, peer_id, "readable").await?;

	let count = owner.services.globals.current_count();
	let profiles = &owner.services.db["useridprofilekey_value"];
	let saved = profiles.qry(&(peer_id, STATUS)).await?;

	profiles.put_raw((peer_id, STATUS), b"not-json");
	for (conn, selection) in [("disabled", Selection::Disabled), ("empty", Selection::Empty)] {
		let response: Value = owner
			.sync_selected(conn, room, None, selection)
			.await?
			.error_for_status()?
			.json()
			.await?;

		assert!(response["extensions"][PROFILES].is_null());
	}

	fails_without_token(owner, "failures", room, Some(pos)).await?;
	profiles.put_raw((peer_id, STATUS), saved.as_ref());

	let repaired = owner
		.sync_profiles("failures", room, Some(pos))
		.await?;

	assert_eq!(update(&repaired, peer_id)[STATUS]["text"], "readable");

	let changes = &owner.services.db["profilechangeid_userid"];
	let key = (room, count, STATUS);
	let recorded = changes.qry(&key).await?;

	assert_eq!(recorded.as_ref(), peer_id.as_bytes());
	changes.put_raw(key, b"not-a-user-id");
	fails_without_token(owner, "failures", room, Some(pos)).await?;
	changes.put_raw(key, peer_id.as_bytes());

	let repaired = owner
		.sync_profiles("failures", room, Some(pos))
		.await?;

	assert_eq!(update(&repaired, peer_id)[STATUS]["text"], "readable");

	let outside = (room, u64::MAX, STATUS);

	changes.put_raw(outside, b"not-a-user-id");

	let bounded = owner
		.sync_profiles("failures", room, Some(pos))
		.await?;

	assert_eq!(update(&bounded, peer_id)[STATUS]["text"], "readable");
	changes.del(outside);

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn membership_failures(
	owner: &Client<'_>,
	owner_id: &UserId,
	peer_id: &UserId,
	room: &str,
) -> Result {
	let members = &owner.services.db["roomuserid_joined"];
	let invalid = (room, "not-a-user-id");

	members.put_raw(invalid, 1_u64.to_be_bytes());
	fails_without_token(owner, "member-key", room, None).await?;
	members.del(invalid);

	let repaired = owner
		.sync_profiles("member-key", room, None)
		.await?;

	assert_eq!(update(&repaired, peer_id)[STATUS]["text"], "readable");

	let key = (room, owner_id);
	let saved = members.qry(&key).await?;

	// Shorter than a u64, so the decoder rejects it in every profile; an
	// over-long value only trips a debug assertion.
	members.put_raw(key, b"short");
	fails_without_token(owner, "member-count", room, None).await?;
	members.put_raw(key, saved.as_ref());

	let repaired = owner
		.sync_profiles("member-count", room, None)
		.await?;

	assert_eq!(update(&repaired, peer_id)[STATUS]["text"], "readable");

	// A join recorded before positions were stored holds an empty value.
	members.put_raw(key, b"");

	let legacy = owner
		.sync_profiles("member-legacy", room, None)
		.await?;

	assert_eq!(update(&legacy, peer_id)[STATUS]["text"], "readable");

	members.put_raw(key, saved.as_ref());

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn cleared_fields(owner: &Client<'_>, peer_id: &UserId, room: &str) -> Result {
	let opening = owner.sync_profiles("cleared", room, None).await?;
	let pos = field(&opening, "pos")?;

	owner
		.services
		.profile
		.set_profile_keys(peer_id, &[(ProfileFieldName::from(STATUS), Some(Value::Null))], None)
		.await?;

	let stored = owner
		.sync_profiles("cleared", room, Some(pos))
		.await?;

	let pos = field(&stored, "pos")?;

	assert!(
		update(&stored, peer_id)
			.get(STATUS)
			.is_some_and(Value::is_null)
	);

	assert!(stored["extensions"][PROFILES]["users"][peer_id.as_str()]["removed"].is_null());

	owner
		.services
		.profile
		.set_profile_keys(peer_id, &[(ProfileFieldName::from(STATUS), None)], None)
		.await?;

	let removed = owner
		.sync_profiles("cleared", room, Some(pos))
		.await?;

	assert_eq!(
		removed["extensions"][PROFILES]["users"][peer_id.as_str()]["removed"],
		json!([STATUS])
	);

	assert!(update(&removed, peer_id).get(STATUS).is_none());

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn departed_profile(
	owner: &Client<'_>,
	peer: &Client<'_>,
	peer_id: &UserId,
	room: &str,
) -> Result {
	let shared = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	peer.post(&format!("rooms/{shared}/join"), &json!({}))
		.await?;

	let opening = owner
		.sync_profiles("departure", room, None)
		.await?;

	let pos = field(&opening, "pos")?;

	set_status(owner.services, peer_id, "shared").await?;
	peer.post(&format!("rooms/{room}/leave"), &json!({}))
		.await?;

	set_status(owner.services, peer_id, "still shared").await?;

	for (conn, selected, pos) in
		[("departure", room, Some(pos)), ("shared-initial", shared.as_str(), None)]
	{
		let response = owner.sync_profiles(conn, selected, pos).await?;

		assert_eq!(update(&response, peer_id)[STATUS]["text"], "still shared");
	}

	peer.post(&format!("rooms/{shared}/leave"), &json!({}))
		.await?;

	set_status(owner.services, peer_id, "private").await?;

	for (conn, selected, pos) in
		[("departure", room, Some(pos)), ("departed-initial", shared.as_str(), None)]
	{
		let response = owner.sync_profiles(conn, selected, pos).await?;

		assert!(update(&response, peer_id).is_null(), "a departed peer leaked its profile");
	}

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn room_admission(
	owner: &Client<'_>,
	peer: &Client<'_>,
	owner_id: &UserId,
	peer_id: &UserId,
) -> Result {
	let shared = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let denied = peer
		.create_room(&json!({
			"preset": "public_chat",
			"initial_state": [{
				"type": "m.room.join_rules", "state_key": "",
				"content": { "join_rule": "knock" }
			}],
		}))
		.await?;

	peer.post(&format!("rooms/{shared}/join"), &json!({}))
		.await?;

	peer.post(&format!("rooms/{denied}/invite"), &json!({ "user_id": owner_id }))
		.await?;

	owner
		.post(&format!("rooms/{denied}/join"), &json!({}))
		.await?;

	let opening = owner
		.sync_profiles("room-admission", denied.as_str(), None)
		.await?;

	let pos = field(&opening, "pos")?;

	owner
		.post(&format!("rooms/{denied}/leave"), &json!({}))
		.await?;

	set_status(owner.services, peer_id, "not joined").await?;

	let departed = owner
		.sync_profiles("room-admission", denied.as_str(), Some(pos))
		.await?;

	assert!(update(&departed, peer_id).is_null());

	peer.post(&format!("rooms/{denied}/invite"), &json!({ "user_id": owner_id }))
		.await?;

	denied_room_change(owner, peer_id, denied.as_str(), "invited").await?;
	owner
		.post(&format!("rooms/{denied}/leave"), &json!({}))
		.await?;

	owner
		.post(&format!("knock/{denied}"), &json!({}))
		.await?;

	denied_room_change(owner, peer_id, denied.as_str(), "knocking").await?;

	let joined = owner
		.sync_profiles("joined-control", shared.as_str(), None)
		.await?;

	assert_eq!(update(&joined, peer_id)[STATUS]["text"], "knocking");

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn denied_room_change(
	owner: &Client<'_>,
	peer_id: &UserId,
	room: &str,
	conn: &str,
) -> Result {
	let opening = owner.sync_profiles(conn, room, None).await?;
	let pos = field(&opening, "pos")?;

	set_status(owner.services, peer_id, conn).await?;

	let response = owner.sync_profiles(conn, room, Some(pos)).await?;

	assert!(update(&response, peer_id).is_null());

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn set_status(services: &Services, user_id: &UserId, text: &str) -> Result {
	let status = json!({ "text": text, "emoji": "" });

	services
		.profile
		.set_profile_keys(user_id, &[(ProfileFieldName::from(STATUS), Some(status))], None)
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn fails_without_token(
	owner: &Client<'_>,
	conn: &str,
	room: &str,
	pos: Option<&str>,
) -> Result {
	let response = owner.sync_response(conn, room, pos).await?;

	assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

	let body: Value = response.json().await?;

	assert!(body.get("pos").is_none());

	Ok(())
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "trace", skip_all)]
async fn sync_profiles(&self, conn: &str, room: &str, pos: Option<&str>) -> Result<Value> {
	self.sync_response(conn, room, pos)
		.await?
		.error_for_status()?
		.json()
		.await
		.map_err(Into::into)
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "trace", skip_all)]
async fn sync_response(&self, conn: &str, room: &str, pos: Option<&str>) -> Result<Response> {
	self.sync_selected(conn, room, pos, Selection::NonLazy)
		.await
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "trace", skip_all)]
async fn sync_selected(
	&self,
	conn: &str,
	room: &str,
	pos: Option<&str>,
	selection: Selection,
) -> Result<Response> {
	let required = match selection {
		| Selection::Lazy => json!([["m.room.member", "$LAZY"]]),
		| _ => json!([]),
	};

	let fields = match selection {
		| Selection::Empty => json!([]),
		| _ => json!([STATUS]),
	};

	let timeline_limit = u32::from(matches!(selection, Selection::Timeline));
	let body = json!({
		"conn_id": conn,
		"lists": {},
		"room_subscriptions": { room: { "required_state": required, "timeline_limit": timeline_limit } },
		"extensions": { PROFILES: { "enabled": !matches!(selection, Selection::Disabled), "fields": fields } },
	});

	let query = pos
		.map(|pos| format!("?pos={pos}&timeout=0"))
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
		.await
		.map_err(Into::into)
}

fn update<'a>(response: &'a Value, user_id: &UserId) -> &'a Value {
	&response["extensions"][PROFILES]["users"][user_id.as_str()]["updated"]
}
