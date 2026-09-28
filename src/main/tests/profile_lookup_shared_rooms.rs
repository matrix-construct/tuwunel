#![cfg(test)]

use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel_core::{Result, err, ruma::UserId};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

mod client;
mod fixture;

const OWNER_TOKEN: &str = "profile-lookup-owner-access-token";
const REQUESTER_TOKEN: &str = "profile-lookup-requester-access-token";

/// Profile reads remain available to their owner and users sharing a room, but
/// unrelated authenticated users receive the same response as for an unknown
/// profile.
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

	assert_profile_status(&requester, &owner_id, StatusCode::NOT_FOUND).await?;
	assert_profile_field_status(&requester, &owner_id, StatusCode::NOT_FOUND).await?;
	assert_profile_status(&owner, &owner_id, StatusCode::OK).await?;

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
