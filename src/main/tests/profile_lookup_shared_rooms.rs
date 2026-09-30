#![cfg(test)]

use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel_core::{Result, err, ruma::UserId};
use tuwunel_service::Services;

use self::{
	appservice::{Bridge, register_appservice},
	client::{Client, register},
	fixture::boot,
};

mod appservice;
mod client;
mod fixture;

const OWNER_TOKEN: &str = "profile-lookup-owner-access-token";
const REQUESTER_TOKEN: &str = "profile-lookup-requester-access-token";
const BRIDGE_TOKEN: &str = "profile-lookup-bridge-access-token";

const BRIDGE: Bridge<'static> = Bridge {
	id: "profile-lookup-shared-rooms",
	token: BRIDGE_TOKEN,
	sender_localpart: "profile_lookup_bridge",
	users: "^@profile_lookup_bridge:.*$",
	aliases: None,
};

/// Profile reads are refused with 403 to users who share no room with the owner.
///
/// The owner, users sharing a room, and appservices can still read them.
#[test]
fn profile_lookups_require_a_shared_room() -> Result {
	let options = [
		"require_auth_for_profile_requests=true",
		"limit_profile_requests_to_users_who_share_rooms=true",
	];

	boot("profile-lookup-shared-rooms", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let owner_id = register(services, "profileowner", OWNER_TOKEN).await?;
	let requester_id = register(services, "profilerequester", REQUESTER_TOKEN).await?;
	let owner = Client { services, base, token: OWNER_TOKEN };
	let requester = Client { services, base, token: REQUESTER_TOKEN };

	assert_profile_status(&requester, &owner_id, StatusCode::FORBIDDEN).await?;
	assert_profile_field_status(&requester, &owner_id, StatusCode::FORBIDDEN).await?;
	assert_profile_status(&owner, &owner_id, StatusCode::OK).await?;

	register_appservice(services, &BRIDGE).await?;

	let bridge = Client { services, base, token: BRIDGE_TOKEN };

	assert_profile_status(&bridge, &owner_id, StatusCode::OK).await?;
	assert_profile_field_status(&bridge, &owner_id, StatusCode::OK).await?;

	let room_id = owner
		.create_room(&json!({ "preset": "private_chat", "invite": [&requester_id] }))
		.await?;

	requester
		.post(&format!("rooms/{room_id}/join"), &json!({}))
		.await?;

	assert_profile_status(&requester, &owner_id, StatusCode::OK).await?;
	assert_profile_field_status(&requester, &owner_id, StatusCode::OK).await
}

async fn assert_profile_status(
	client: &Client<'_>,
	user_id: &UserId,
	expected: StatusCode,
) -> Result {
	assert_status(client, &format!("profile/{user_id}"), expected).await
}

async fn assert_profile_field_status(
	client: &Client<'_>,
	user_id: &UserId,
	expected: StatusCode,
) -> Result {
	assert_status(client, &format!("profile/{user_id}/displayname"), expected).await
}

async fn assert_status(client: &Client<'_>, path: &str, expected: StatusCode) -> Result {
	let response = client
		.services
		.client
		.clients
		.default
		.get(client.url(path))
		.bearer_auth(client.token)
		.send()
		.await?;

	let status = response.status();
	let body: Value = response.json().await?;

	if status != expected {
		return Err(err!("{path} returned {status} with {body}, expected {expected}"));
	}

	Ok(())
}
