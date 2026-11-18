#![cfg(test)]

//! MatrixRTC transport discovery (MSC4143) and an RTC slot at room creation, on
//! a booted server.
//!
//! Both discovery paths answer a member and a guest with the configured
//! transports in the server's order of preference, refuse a caller with no
//! access token, and are advertised by `/versions`.

use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{OwnedUserId, events::StateEventType},
};
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, register},
	fixture::boot,
	require::require,
};

mod client;
mod fixture;

#[expect(
	dead_code,
	reason = "Only the raw GET is used, so each case judges the status."
)]
#[path = "client/get.rs"]
mod get;

#[path = "client/require.rs"]
mod require;

const TOKEN: &str = "rtc-transports-member-access-token";
const OPTIONS: [&str; 5] = [
	r#"well_known.rtc_transports=[{type="org.example.sfu",sfu_url="https://sfu.example.org"}]"#,
	r#"well_known.livekit_url="https://livekit.example.org""#,
	"registration.enable=true",
	r#"registration.token="rtc-transports-registration-token""#,
	"registration.guests.enable=true",
];

// Each discovery path's prefix under the client API root, and its response key.
const PATHS: [(&str, &str); 2] =
	[("v1/", "transports"), ("unstable/org.matrix.msc4143/", "rtc_transports")];

/// Runs every case on one server.
///
/// The custom transport is configured ahead of `livekit_url`, so both answers
/// must list it first.
#[test]
fn rtc_transports() -> Result { boot("rtc-transports", OPTIONS, exercise) }

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "rtcmember", TOKEN).await?;

	let guest_token = register_guest(services, base).await?;
	let member = Client { services, base, token: TOKEN };
	let guest = Client { services, base, token: &guest_token };

	for (prefix, key) in PATHS {
		let url = member.client_url(prefix, "rtc/transports");

		offers_transports_in_order(&member, &url, key).await?;
		offers_transports_in_order(&guest, &url, key).await?;
		refuses_a_missing_token(services, &url).await?;
	}

	advertises_both_paths(&member).await?;
	creates_a_room_with_a_slot(services, &member).await
}

/// Register a guest through the client API and return its access token.
///
/// The account is checked to be a guest, so the discovery cases run with a
/// guest's token and not a member's.
#[tracing::instrument(level = "debug", skip_all)]
async fn register_guest(services: &Services, base: &str) -> Result<String> {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/register?kind=guest"))
		.json(&json!({}))
		.send()
		.await?;

	let body = accepted(response).await?;
	let user_id: OwnedUserId = field(&body, "user_id")?.try_into()?;

	require(services.users.is_guest(&user_id).await, "a guest registration made no guest")?;

	let token = field(&body, "access_token")?.to_owned();

	Ok(token)
}

/// A caller receives every configured transport, the custom one ahead of
/// `livekit_url`, under the path's response key.
///
/// The stable path answers under `transports` and the unstable path under
/// `rtc_transports`, each with the same list.
#[tracing::instrument(level = "debug", skip_all)]
async fn offers_transports_in_order(client: &Client<'_>, url: &str, key: &str) -> Result {
	let expected = json!([
		{ "type": "org.example.sfu", "sfu_url": "https://sfu.example.org" },
		{ "type": "livekit", "livekit_service_url": "https://livekit.example.org" },
	]);

	let body = accepted(client.get_url(url).await?).await?;

	require(body[key] == expected, &format!("{url} answered {body}, expected {expected}"))
}

/// A request with no access token is refused as unauthenticated.
///
/// Both paths answer 401 with `M_MISSING_TOKEN`, so the transport list is never
/// served to an anonymous caller.
#[tracing::instrument(level = "debug", skip_all)]
async fn refuses_a_missing_token(services: &Services, url: &str) -> Result {
	let response = services
		.client
		.clients
		.default
		.get(url)
		.send()
		.await?;

	let status = response.status();
	let body: Value = response.json().await?;

	require(
		status == StatusCode::UNAUTHORIZED && body["errcode"] == "M_MISSING_TOKEN",
		&format!("{url} answered a missing token with {status}: {body}"),
	)
}

/// `/versions` advertises the unstable path and, with `.stable`, the v1 path.
///
/// A client reads these flags to pick a discovery path, so each must be
/// present and `true`.
#[tracing::instrument(level = "debug", skip_all)]
async fn advertises_both_paths(member: &Client<'_>) -> Result {
	let versions = member
		.get_url(&member.client_url("", "versions"))
		.await?;

	let versions = accepted(versions).await?;
	let features = &versions["unstable_features"];

	for feature in ["org.matrix.msc4143", "org.matrix.msc4143.stable"] {
		require(features[feature] == true, &format!("/versions does not advertise {feature}"))?;
	}

	Ok(())
}

/// A room created with an `m.rtc.slot` in its initial state holds that slot.
///
/// The slot is read back from the room's current state, so the event passed
/// every check createRoom applies to initial state.
#[tracing::instrument(level = "debug", skip_all)]
async fn creates_a_room_with_a_slot(services: &Services, member: &Client<'_>) -> Result {
	let slot = json!({ "status": "open", "application": { "type": "m.call" } });
	let body = json!({
		"preset": "private_chat",
		"initial_state": [{ "type": "m.rtc.slot", "state_key": "m.call#room", "content": slot }],
	});

	let room = member.create_room(&body).await?;
	let content = services
		.state_accessor
		.room_state_get_content::<Value>(
			&room,
			&StateEventType::from("m.rtc.slot"),
			"m.call#room",
		)
		.await?;

	require(content == slot, &format!("the room holds {content}, expected {slot}"))
}
