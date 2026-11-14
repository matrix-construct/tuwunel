#![cfg(test)]

use futures::StreamExt;
use reqwest::Response;
use serde_json::{Value, json};
use tuwunel_core::{
	Result, err,
	http::{HeaderValue, Request, header::AUTHORIZATION},
	ruma::{
		RoomId, UserId,
		api::{
			appservice::{Namespace, Namespaces, Registration, RegistrationInit},
			federation::authentication::{ServerSignaturesInput, XMatrix},
		},
		events::room::member::MembershipState,
	},
};
use tuwunel_service::{Services, appservice::IS_JOINED_SCOPE};

use self::{
	client::{Client, accepted, field, register},
	fixture::boot,
	state_cache::membership_update,
};

mod client;
mod fixture;

#[expect(dead_code)] // Only the raw GET is used from the GET add-on.
#[path = "client/get.rs"]
mod get;

#[path = "fixture/state_cache.rs"]
mod state_cache;

const OWNER_TOKEN: &str = "appservice-is-joined-owner-token";
const OUTSIDER_TOKEN: &str = "appservice-is-joined-outsider-token";
const SCOPED_TOKEN: &str = "appservice-is-joined-scoped-token";
const UNSCOPED_TOKEN: &str = "appservice-is-joined-unscoped-token";
const INLINE_TOKEN: &str = "appservice-is-joined-inline-token";
const REMOTE: &str = "remote.invalid";
const STABLE_SCOPE: &str = "urn:matrix:client:rooms:is_joined";

/// MSC4502: only an appservice holding the scope learns room membership.
///
/// A query for a local or remote user or server answers from the room's
/// records, and a query for an unknown subject or room answers `false`. A
/// masquerading appservice keeps its scope, while a user token or an unscoped
/// appservice is refused. Once this server leaves, a query for a remote member
/// still recorded as joined answers `false`. An unknown scope is ignored at
/// load and refused at registration, and the `.*` namespace these
/// registrations claim admits federated to-device only for existing accounts.
#[test]
fn scoped_appservice_queries_room_membership() -> Result {
	let token = format!(r#"appservice.inline.as_token="{INLINE_TOKEN}""#);
	let scopes = format!(r#"appservice.inline.scopes=["{IS_JOINED_SCOPE}","{STABLE_SCOPE}"]"#);
	let inline = [
		token.as_str(),
		r#"appservice.inline.hs_token="appservice-is-joined-inline-hs-token""#,
		r#"appservice.inline.sender_localpart="inline_bot""#,
		scopes.as_str(),
	];

	boot("appservice-is-joined", inline, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let owner_id = register(services, "is_joined_owner", OWNER_TOKEN).await?;
	let outsider_id = register(services, "is_joined_outsider", OUTSIDER_TOKEN).await?;

	register_appservice(services, "scoped", SCOPED_TOKEN, &[IS_JOINED_SCOPE]).await?;
	register_appservice(services, "unscoped", UNSCOPED_TOKEN, &[]).await?;

	let unknown =
		register_appservice(services, "unknown", "is-joined-unknown", &[STABLE_SCOPE]).await;

	assert!(unknown.is_err(), "registering an unknown scope succeeded");

	let owner = Client { services, base, token: OWNER_TOKEN };
	let scoped = Client { services, base, token: SCOPED_TOKEN };
	let unscoped = Client { services, base, token: UNSCOPED_TOKEN };
	let inline = Client { services, base, token: INLINE_TOKEN };
	let ours = services.globals.server_name();
	let room_id = owner.create_room(&json!({})).await?;
	let remote_id = UserId::parse(format!("@remote:{REMOTE}"))?;

	services
		.state_cache
		.update_membership(membership_update(
			&room_id,
			&remote_id,
			MembershipState::Join,
			*services.globals.next_count(),
		))
		.await?;

	for (query, expected) in [
		(format!("mxid={owner_id}"), true),
		(format!("mxid={outsider_id}"), false),
		(format!("mxid={remote_id}"), true),
		(format!("mxid=@stranger:{REMOTE}"), false),
		(format!("server_name={ours}"), true),
		(format!("server_name={REMOTE}"), true),
		("server_name=unknown.invalid".to_owned(), false),
		(format!("mxid={owner_id}&user_id=@lk_alice:{ours}"), true),
	] {
		assert_eq!(joined(&scoped, &room_id, &query).await?, expected, "{query}");
	}

	let unknown_room = RoomId::parse(format!("!unknown:{ours}"))?;
	let own = format!("mxid={owner_id}");
	let both = format!("{own}&server_name={ours}");

	assert!(!joined(&scoped, &unknown_room, &own).await?, "an unknown room is joined");
	assert!(joined(&inline, &room_id, &own).await?, "the inline scope was not granted");

	for (client, query, expected) in [
		(&scoped, both.as_str(), (400, "M_MISSING_PARAM")),
		(&scoped, "", (400, "M_MISSING_PARAM")),
		(&scoped, "mxid=not-a-user-id", (400, "M_BAD_JSON")),
		(&owner, own.as_str(), (403, "M_FORBIDDEN")),
		(&unscoped, own.as_str(), (403, "M_FORBIDDEN")),
	] {
		let (status, errcode) = refusal(client, &room_id, query).await?;

		assert_eq!((status, errcode.as_str()), expected, "{query}");
	}

	owner
		.post(&format!("rooms/{room_id}/leave"), &json!({}))
		.await?;

	assert!(
		services
			.state_cache
			.is_joined(&remote_id, &room_id)
			.await,
		"the remote member's join outlives our departure"
	);

	for query in [format!("mxid={remote_id}"), format!("server_name={REMOTE}"), own] {
		assert!(!joined(&scoped, &room_id, &query).await?, "{query} after our server left");
	}

	let versions = owner
		.get_url(&owner.client_url("", "versions"))
		.await?;

	let advertised = accepted(versions).await?["unstable_features"]["io.element.msc4502"] == true;

	assert!(advertised, "/versions does not advertise io.element.msc4502");

	federated_to_device(services, base, &owner_id).await
}

/// Deliver one federated to-device event to three local ids and count the
/// rows each got.
///
/// The transaction comes from this server itself, signed with its own key as a
/// peer would sign it. Every id matches the `.*` namespace; only the one with
/// no account gets nothing.
async fn federated_to_device(services: &Services, base: &str, owner_id: &UserId) -> Result {
	let ours = services.globals.server_name();
	let ghost = UserId::parse(format!("@is_joined_ghost:{ours}"))?;
	let bot = UserId::parse(format!("@scoped_bot:{ours}"))?;
	let messages = json!({
		ghost.as_str(): { "GHOST": { "n": 1 } },
		bot.as_str(): { "BOT": { "n": 1 } },
		owner_id.as_str(): { "OWNER": { "n": 1 } },
	});

	let edu = json!({
		"edu_type": "m.direct_to_device",
		"content": {
			"sender": owner_id,
			"type": "org.example.is_joined",
			"message_id": "is-joined-to-device",
			"messages": messages,
		},
	});

	let body = json!({ "origin": ours, "origin_server_ts": 1, "pdus": [], "edus": [edu] });
	let uri = format!("{base}/_matrix/federation/v1/send/is-joined-to-device");
	let body = serde_json::to_string(&body)?;
	let request = Request::builder()
		.method("PUT")
		.uri(&uri)
		.body(body.as_str())?;

	let input = ServerSignaturesInput::new(
		ours.to_owned(),
		ours.to_owned(),
		services.server_keys.keypair(),
	);

	let signature = XMatrix::try_from_http_request(&request, input)
		.map_err(|e| err!("signing the transaction failed: {e}"))?;

	let response = services
		.client
		.clients
		.default
		.put(&uri)
		.header(AUTHORIZATION, HeaderValue::from(&signature))
		.body(body)
		.send()
		.await?;

	accepted(response).await?;

	for (user_id, device_id, expected) in
		[(&*ghost, "GHOST", 0), (&*bot, "BOT", 1), (owner_id, "OWNER", 1)]
	{
		let rows = services
			.users
			.get_to_device_events(user_id, device_id.into(), None, None)
			.count()
			.await;

		assert_eq!(rows, expected, "{user_id}");
	}

	Ok(())
}

/// Register an appservice shaped like lk-jwt-service's registration.
///
/// It has no URL and claims every local user without exclusivity.
async fn register_appservice(
	services: &Services,
	id: &str,
	token: &str,
	scopes: &[&str],
) -> Result {
	let base: Registration = RegistrationInit {
		id: id.to_owned(),
		url: None,
		as_token: token.to_owned(),
		hs_token: format!("{id}-hs-token"),
		sender_localpart: format!("{id}_bot"),
		namespaces: Namespaces {
			users: vec![Namespace::new(false, ".*".to_owned())],
			..Default::default()
		},
		rate_limited: None,
		protocols: None,
	}
	.into();

	let scopes = scopes
		.iter()
		.copied()
		.map(ToOwned::to_owned)
		.collect();

	let registration = Registration { scopes, ..base };

	services
		.appservice
		.register_appservice(registration)
		.await
}

async fn joined(client: &Client<'_>, room_id: &RoomId, query: &str) -> Result<bool> {
	let response = get_is_joined(client, room_id, query).await?;

	accepted(response)
		.await?
		.get("joined")
		.and_then(Value::as_bool)
		.ok_or_else(|| err!("response omitted joined"))
}

async fn refusal(client: &Client<'_>, room_id: &RoomId, query: &str) -> Result<(u16, String)> {
	let response = get_is_joined(client, room_id, query).await?;
	let status = response.status().as_u16();
	let body: Value = response.json().await?;

	Ok((status, field(&body, "errcode")?.to_owned()))
}

async fn get_is_joined(client: &Client<'_>, room_id: &RoomId, query: &str) -> Result<Response> {
	let rest = format!("rooms/{room_id}/is_joined?{query}");
	let url = client.client_url("unstable/io.element.msc4502/", &rest);

	client.get_url(&url).await
}
