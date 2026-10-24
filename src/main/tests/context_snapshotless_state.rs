#![cfg(test)]

use reqwest::StatusCode;
use serde_json::json;
use tuwunel_core::{
	Result,
	ruma::{EventId, OwnedRoomId, UserId, events::StateEventType},
};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	context::{membership_of, topic_of},
	fixture::boot,
	room_state::state_id,
};

mod client;
mod fixture;
mod room_state;

#[path = "client/admin_api.rs"]
mod admin_api;

#[path = "client/context.rs"]
mod context;

#[expect(dead_code)] // Only the raw GET is used from the GET add-on.
#[path = "client/get.rs"]
mod get;

#[path = "client/membership.rs"]
mod membership;

#[path = "client/put.rs"]
mod put;

#[path = "client/state.rs"]
mod state;

#[path = "client/topic.rs"]
mod topic;

const OWNER_TOKEN: &str = "context-snapshotless-state-owner-access-token";
const KICKED_TOKEN: &str = "context-snapshotless-state-kicked-access-token";
const FORGETTER_TOKEN: &str = "context-snapshotless-state-forgetter-access-token";
const NEWCOMER_TOKEN: &str = "context-snapshotless-state-newcomer-access-token";
const STRANGER_TOKEN: &str = "context-snapshotless-state-stranger-access-token";
const ADMIN_TOKEN: &str = "context-snapshotless-state-admin-access-token";

/// Context around an event without recorded state carries only state the
/// requester may read.
///
/// A room's create event has no snapshot of its own. A never-member and a
/// member who forgot the room are refused the context, the owner reads the
/// current state, and a kicked member reads the state as of each departure,
/// without the member who joined later. An admin reads the current state
/// through the admin API without joining, and the state as of their own
/// departure once they have left. Each other history visibility then gets a
/// room of its own.
#[test]
fn context_without_snapshot_serves_only_readable_state() -> Result {
	let options = ["create_admin_room=true", "grant_admin_to_first_user=false"];

	boot("context-snapshotless-state", options, exercise)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	let kicked_id = register(services, "contextkicked", KICKED_TOKEN).await?;
	let forgetter_id = register(services, "contextforgetter", FORGETTER_TOKEN).await?;
	let newcomer_id = register(services, "contextnewcomer", NEWCOMER_TOKEN).await?;
	let admin_id = register(services, "contextadmin", ADMIN_TOKEN).await?;

	register(services, "contextowner", OWNER_TOKEN).await?;
	register(services, "contextstranger", STRANGER_TOKEN).await?;
	services.admin.make_user_admin(&admin_id).await?;

	let client = |token| Client { services, base, token };
	let owner = client(OWNER_TOKEN);
	let kicked = client(KICKED_TOKEN);
	let forgetter = client(FORGETTER_TOKEN);
	let newcomer = client(NEWCOMER_TOKEN);
	let stranger = client(STRANGER_TOKEN);
	let admin = client(ADMIN_TOKEN);

	let room_id = owner
		.create_room(&json!({ "preset": "private_chat", "topic": "before" }))
		.await?;

	let create_id = state_id(services, &room_id, &StateEventType::RoomCreate, "").await?;

	owner.admit(&room_id, &kicked, &kicked_id).await?;
	owner
		.admit(&room_id, &forgetter, &forgetter_id)
		.await?;

	owner.act_on(&room_id, "kick", &kicked_id).await?;
	forgetter.act(&room_id, "leave").await?;
	forgetter.act(&room_id, "forget").await?;
	owner.set_topic(&room_id, "after").await?;
	owner
		.admit(&room_id, &newcomer, &newcomer_id)
		.await?;

	let (stranger_status, stranger_reply) = stranger.context(&room_id, &create_id, 0).await?;
	let (forgetter_status, forgetter_reply) = forgetter.context(&room_id, &create_id, 0).await?;
	let (owner_status, owner_reply) = owner.context(&room_id, &create_id, 0).await?;
	let (kicked_status, kicked_reply) = kicked.context(&room_id, &create_id, 0).await?;
	let admin_reply = admin
		.admin_context(&room_id, &create_id, 0)
		.await?;

	assert_eq!(
		stranger_status,
		StatusCode::NOT_FOUND,
		"a never-member's context: {stranger_reply}"
	);

	assert_eq!(stranger_reply["errcode"], "M_NOT_FOUND", "{stranger_reply}");
	assert_eq!(
		forgetter_status,
		StatusCode::NOT_FOUND,
		"a forgotten room's context: {forgetter_reply}"
	);

	assert_eq!(owner_status, StatusCode::OK, "{owner_reply}");
	assert_eq!(
		topic_of(&owner_reply),
		Some("after"),
		"owner reads the old state: {owner_reply}"
	);

	assert_eq!(membership_of(&owner_reply, &newcomer_id), Some("join"), "{owner_reply}");

	assert_eq!(kicked_status, StatusCode::OK, "{kicked_reply}");
	assert_eq!(
		topic_of(&kicked_reply),
		Some("before"),
		"kicked member reads later state: {kicked_reply}"
	);

	assert_eq!(membership_of(&kicked_reply, &kicked_id), Some("leave"), "{kicked_reply}");
	assert_eq!(
		membership_of(&kicked_reply, &newcomer_id),
		None,
		"kicked member reads a later member"
	);

	assert_eq!(
		topic_of(&admin_reply),
		Some("after"),
		"admin reads the old state: {admin_reply}"
	);

	assert_eq!(membership_of(&admin_reply, &newcomer_id), Some("join"), "{admin_reply}");

	owner.admit(&room_id, &kicked, &kicked_id).await?;
	owner.set_topic(&room_id, "rejoined").await?;
	owner.act_on(&room_id, "kick", &kicked_id).await?;
	owner.set_topic(&room_id, "final").await?;

	let (rekicked_status, rekicked_reply) = kicked.context(&room_id, &create_id, 0).await?;

	assert_eq!(rekicked_status, StatusCode::OK, "{rekicked_reply}");
	assert_eq!(
		topic_of(&rekicked_reply),
		Some("rejoined"),
		"a member kicked twice misses the second departure's state: {rekicked_reply}"
	);

	owner.admit(&room_id, &admin, &admin_id).await?;
	admin.act(&room_id, "leave").await?;
	owner.set_topic(&room_id, "closing").await?;

	let departed_admin_reply = admin
		.admin_context(&room_id, &create_id, 0)
		.await?;

	assert_eq!(
		topic_of(&departed_admin_reply),
		Some("final"),
		"an admin who left misses their departure's state: {departed_admin_reply}"
	);

	world_readable(services, &owner, &stranger).await?;
	joined_history(services, &owner, (&kicked, &kicked_id)).await?;

	invited_history(services, &owner, (&forgetter, &forgetter_id)).await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn world_readable(
	services: &Services,
	owner: &Client<'_>,
	stranger: &Client<'_>,
) -> Result {
	let room_id = history_room(owner, "world_readable", "open").await?;
	let create_id = state_id(services, &room_id, &StateEventType::RoomCreate, "").await?;
	let (status, reply) = stranger.context(&room_id, &create_id, 0).await?;

	assert_eq!(status, StatusCode::OK, "a stranger's world-readable context: {reply}");
	assert_eq!(topic_of(&reply), Some("open"), "a stranger misses the state: {reply}");

	Ok(())
}

/// In a `joined` room, nothing shows a former member joined at the create
/// event, so one who left is refused its context.
///
/// One who also forgot the room still sees their own join, and with that
/// event's snapshot gone, as a backfilled event's is, reads no state with it.
#[tracing::instrument(level = "debug", skip_all)]
async fn joined_history(
	services: &Services,
	owner: &Client<'_>,
	(member, member_id): (&Client<'_>, &UserId),
) -> Result {
	let room_id = history_room(owner, "joined", "joined").await?;
	let create_id = state_id(services, &room_id, &StateEventType::RoomCreate, "").await?;

	owner.admit(&room_id, member, member_id).await?;

	let member_key = member_id.as_str();
	let join_id = state_id(services, &room_id, &StateEventType::RoomMember, member_key).await?;

	member.act(&room_id, "leave").await?;

	let (left_status, left_reply) = member.context(&room_id, &create_id, 0).await?;

	member.act(&room_id, "forget").await?;
	unrecord_state(services, &join_id).await?;

	let (own_status, own_reply) = member.context(&room_id, &join_id, 0).await?;

	assert_eq!(
		left_status,
		StatusCode::NOT_FOUND,
		"a former member's joined context: {left_reply}"
	);

	assert_eq!(own_status, StatusCode::OK, "{own_reply}");
	assert_eq!(own_reply["event"]["event_id"], join_id.as_str(), "{own_reply}");
	assert!(
		own_reply["state"]
			.as_array()
			.is_none_or(Vec::is_empty),
		"a forgotten room's state around an unrecorded event: {own_reply}"
	);

	Ok(())
}

/// In an `invited` room, a former member invited back is a pending invitee
/// again, and reads the create event's context with the current state.
///
/// The invite cleared the departure a snapshot would be chosen by, so this is
/// the state `/state` serves an invitee there.
#[tracing::instrument(level = "debug", skip_all)]
async fn invited_history(
	services: &Services,
	owner: &Client<'_>,
	(member, member_id): (&Client<'_>, &UserId),
) -> Result {
	let room_id = history_room(owner, "invited", "invited").await?;
	let create_id = state_id(services, &room_id, &StateEventType::RoomCreate, "").await?;

	owner.admit(&room_id, member, member_id).await?;
	owner.act_on(&room_id, "kick", member_id).await?;
	owner.set_topic(&room_id, "invited back").await?;
	owner
		.act_on(&room_id, "invite", member_id)
		.await?;

	let (status, reply) = member.context(&room_id, &create_id, 0).await?;

	assert_eq!(status, StatusCode::OK, "a re-invited former member's context: {reply}");
	assert_eq!(topic_of(&reply), Some("invited back"), "a pending invitee's state: {reply}");

	Ok(())
}

async fn history_room(owner: &Client<'_>, visibility: &str, topic: &str) -> Result<OwnedRoomId> {
	let visibility = json!({
		"type": "m.room.history_visibility",
		"state_key": "",
		"content": { "history_visibility": visibility },
	});

	owner
		.create_room(&json!({
			"preset": "private_chat",
			"topic": topic,
			"initial_state": [visibility],
		}))
		.await
}

/// Drop the snapshot recorded for an event, as a backfilled event has none.
///
/// The event itself stays in the timeline, so only its state goes missing.
async fn unrecord_state(services: &Services, event_id: &EventId) -> Result {
	let shorteventid = services.short.get_shorteventid(event_id).await?;

	services.db["shorteventid_shortstatehash"].remove(&shorteventid.to_be_bytes());

	Ok(())
}
