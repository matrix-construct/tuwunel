#![cfg(test)]

use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel_core::{
	Result, implement,
	itertools::Itertools,
	ruma::{RoomId, UserId},
};
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, register},
	fixture::boot,
};

mod client;
mod fixture;

#[path = "client/get.rs"]
mod get;

#[path = "client/membership.rs"]
mod membership;

#[path = "client/put.rs"]
mod put;

#[path = "client/summary.rs"]
mod summary;

const OWNER_TOKEN: &str = "state-departed-member-owner-access-token";
const KICKED_TOKEN: &str = "state-departed-member-kicked-access-token";
const BANNED_TOKEN: &str = "state-departed-member-banned-access-token";
const FORGETTER_TOKEN: &str = "state-departed-member-forgetter-access-token";
const PREBANNED_TOKEN: &str = "state-departed-member-prebanned-access-token";
const NEWCOMER_TOKEN: &str = "state-departed-member-newcomer-access-token";
const STRANGER_TOKEN: &str = "state-departed-member-stranger-access-token";

const NAME_PATH: &str = "state/m.room.name/";
const MARKER_PATH: &str = "state/org.example.marker/";

/// A former member reads the room state from when they left.
///
/// After one member is kicked, another banned and a third leaves and forgets
/// the room, the room is renamed and a newcomer invited. The kicked and banned
/// members read the state and members as of their departure. An `at` token
/// before the kick shows the kicked member joined, and one after it reads no
/// further than the kick. The forgetter, a stranger and a never-member who was
/// banned are refused, as is the kicked member for the summary and aliases,
/// which show the room as it is now.
#[test]
fn departed_member_reads_state_at_departure() -> Result {
	let options: [&str; 0] = [];

	boot("state-departed-member", options, exercise)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	let owner_id = register(services, "stateowner", OWNER_TOKEN).await?;
	let kicked_id = register(services, "statekicked", KICKED_TOKEN).await?;
	let banned_id = register(services, "statebanned", BANNED_TOKEN).await?;
	let forgetter_id = register(services, "stateforgetter", FORGETTER_TOKEN).await?;
	let prebanned_id = register(services, "stateprebanned", PREBANNED_TOKEN).await?;
	let newcomer_id = register(services, "statenewcomer", NEWCOMER_TOKEN).await?;

	register(services, "statestranger", STRANGER_TOKEN).await?;

	let client = |token| Client { services, base, token };
	let owner = client(OWNER_TOKEN);
	let kicked = client(KICKED_TOKEN);
	let banned = client(BANNED_TOKEN);
	let forgetter = client(FORGETTER_TOKEN);
	let prebanned = client(PREBANNED_TOKEN);
	let newcomer = client(NEWCOMER_TOKEN);
	let stranger = client(STRANGER_TOKEN);

	let room_id = owner
		.create_room(&json!({ "preset": "private_chat", "name": "before" }))
		.await?;

	let path = |endpoint: &str| room_path(&room_id, endpoint);

	owner
		.put(&path(MARKER_PATH), &json!({ "body": "before" }))
		.await?;

	admit(&owner, &kicked, &kicked_id, &room_id).await?;
	admit(&owner, &banned, &banned_id, &room_id).await?;
	admit(&owner, &forgetter, &forgetter_id, &room_id).await?;

	let joined_at = services.globals.current_count();

	owner.act_on(&room_id, "kick", &kicked_id).await?;
	owner.act_on(&room_id, "ban", &banned_id).await?;
	owner
		.act_on(&room_id, "ban", &prebanned_id)
		.await?;

	forgetter.post(&path("leave"), &json!({})).await?;
	forgetter
		.post(&path("forget"), &json!({}))
		.await?;

	owner
		.put(&path(NAME_PATH), &json!({ "name": "after" }))
		.await?;

	owner
		.put(&path(MARKER_PATH), &json!({ "body": "after" }))
		.await?;

	owner
		.act_on(&room_id, "invite", &newcomer_id)
		.await?;

	let latest = services.globals.current_count();

	let kicked_name = kicked.get(&path(NAME_PATH)).await?;
	let kicked_state = kicked.get(&path("state")).await?;
	let kicked_members = kicked.get(&path("members")).await?;
	let kicked_members_at_latest = kicked
		.get(&path(&format!("members?at={latest}")))
		.await?;

	let kicked_members_at_joined = kicked
		.get(&path(&format!("members?at={joined_at}")))
		.await?;

	let kicked_aliases = kicked.status(&path("aliases")).await?;
	let kicked_summary = kicked.summary(&room_id).await?.status();
	let banned_name = banned.get(&path(NAME_PATH)).await?;
	let banned_marker = banned.get(&path(MARKER_PATH)).await?;
	let banned_members = banned.get(&path("members")).await?;
	let forgetter_state = forgetter.status(&path("state")).await?;
	let forgetter_name = forgetter.status(&path(NAME_PATH)).await?;
	let forgetter_members = forgetter.status(&path("members")).await?;
	let prebanned_state = prebanned.status(&path("state")).await?;
	let prebanned_members = prebanned.status(&path("members")).await?;
	let stranger_state = stranger.status(&path("state")).await?;
	let owner_name = owner.get(&path(NAME_PATH)).await?;
	let owner_members = owner.get(&path("members")).await?;
	let owner_summary = accepted(owner.summary(&room_id).await?).await?;
	let newcomer_summary = accepted(newcomer.summary(&room_id).await?).await?;

	let names: Vec<_> = events(&kicked_state)
		.filter(|event| event["type"] == "m.room.name")
		.filter_map(|event| event["content"]["name"].as_str())
		.collect();

	assert_eq!(kicked_name["name"], "before", "kicked member reads the new name");
	assert_eq!(names, ["before"], "kicked member's state has the new name: {kicked_state}");
	assert_eq!(membership(&kicked_members, &kicked_id), Some("leave"));
	assert_eq!(membership(&kicked_members, &newcomer_id), None, "{kicked_members}");
	assert_eq!(
		memberships(&kicked_members),
		memberships(&kicked_members_at_latest),
		"a later token reads past the kick"
	);

	assert_eq!(membership(&kicked_members_at_joined, &kicked_id), Some("join"));
	assert_eq!(kicked_aliases, StatusCode::FORBIDDEN, "kicked member reads the aliases");
	assert_eq!(kicked_summary, StatusCode::FORBIDDEN, "kicked member reads the summary");

	assert_eq!(banned_name["name"], "before", "banned member reads the new name");
	assert_eq!(banned_marker["body"], "before", "banned member reads the new state");
	assert_eq!(membership(&banned_members, &banned_id), Some("ban"));
	assert_eq!(membership(&banned_members, &owner_id), Some("join"));
	assert_eq!(membership(&banned_members, &newcomer_id), None, "{banned_members}");

	assert_eq!(forgetter_state, StatusCode::FORBIDDEN, "forgotten room's state");
	assert_eq!(forgetter_name, StatusCode::NOT_FOUND, "forgotten room's name");
	assert_eq!(forgetter_members, StatusCode::FORBIDDEN, "forgotten room's members");
	assert_eq!(prebanned_state, StatusCode::FORBIDDEN, "a never-member's ban reads the state");
	assert_eq!(prebanned_members, StatusCode::FORBIDDEN, "a never-member's ban reads members");
	assert_eq!(stranger_state, StatusCode::FORBIDDEN, "a stranger reads the state");

	assert_eq!(owner_name["name"], "after", "owner reads the old name");
	assert_eq!(membership(&owner_members, &newcomer_id), Some("invite"));
	assert_eq!(owner_summary["name"], "after", "owner's summary: {owner_summary}");
	assert_eq!(newcomer_summary["membership"], "invite", "{newcomer_summary}");

	world_readable(&owner, (&kicked, &kicked_id), (&forgetter, &forgetter_id), &stranger).await
}

/// Get one endpoint path as this user and keep only the reply's status.
///
/// A refusal is a status to assert, not an error.
#[implement(Client, params = "<'_>")]
async fn status(&self, path: &str) -> Result<StatusCode> {
	Ok(self.get_url(&self.url(path)).await?.status())
}

/// In a world-readable room, only a departed member reads the old state.
///
/// A stranger, and a former member who forgot the room, read it as it is now.
async fn world_readable(
	owner: &Client<'_>,
	(kicked, kicked_id): (&Client<'_>, &UserId),
	(forgetter, forgetter_id): (&Client<'_>, &UserId),
	stranger: &Client<'_>,
) -> Result {
	let visibility = json!({
		"type": "m.room.history_visibility",
		"state_key": "",
		"content": { "history_visibility": "world_readable" },
	});

	let room_id = owner
		.create_room(&json!({
			"preset": "private_chat",
			"name": "before",
			"initial_state": [visibility],
		}))
		.await?;

	let path = |endpoint: &str| room_path(&room_id, endpoint);

	admit(owner, kicked, kicked_id, &room_id).await?;
	admit(owner, forgetter, forgetter_id, &room_id).await?;

	owner.act_on(&room_id, "kick", kicked_id).await?;
	forgetter.post(&path("leave"), &json!({})).await?;
	forgetter
		.post(&path("forget"), &json!({}))
		.await?;

	owner
		.put(&path(NAME_PATH), &json!({ "name": "after" }))
		.await?;

	let kicked_name = kicked.get(&path(NAME_PATH)).await?;
	let forgetter_name = forgetter.get(&path(NAME_PATH)).await?;
	let stranger_name = stranger.get(&path(NAME_PATH)).await?;

	assert_eq!(kicked_name["name"], "before", "kicked member reads the new name");
	assert_eq!(forgetter_name["name"], "after", "forgotten room reads the old name");
	assert_eq!(stranger_name["name"], "after", "stranger reads the old name");

	Ok(())
}

async fn admit(
	owner: &Client<'_>,
	invitee: &Client<'_>,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	owner.act_on(room_id, "invite", user_id).await?;
	invitee
		.post(&room_path(room_id, "join"), &json!({}))
		.await?;

	Ok(())
}

fn room_path(room_id: &RoomId, endpoint: &str) -> String { format!("rooms/{room_id}/{endpoint}") }

fn membership<'a>(members: &'a Value, user_id: &UserId) -> Option<&'a str> {
	events(&members["chunk"])
		.find(|event| event["state_key"] == user_id.as_str())
		.and_then(|event| event["content"]["membership"].as_str())
}

/// Every user and membership a `/members` reply records, in user order.
///
/// The reply lists its members in no fixed order, so replies compare by this.
fn memberships(members: &Value) -> Vec<(&str, &str)> {
	events(&members["chunk"])
		.filter_map(|event| {
			event["state_key"]
				.as_str()
				.zip(event["content"]["membership"].as_str())
		})
		.sorted_unstable()
		.collect()
}

fn events(value: &Value) -> impl Iterator<Item = &Value> {
	value.as_array().into_iter().flatten()
}
