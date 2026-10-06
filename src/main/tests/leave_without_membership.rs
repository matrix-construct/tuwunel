#![cfg(test)]

use serde_json::json;
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

mod client;
mod fixture;

const OWNER_TOKEN: &str = "leave-without-membership-owner-access-token";
const MEMBER_TOKEN: &str = "leave-without-membership-member-access-token";
const STRANGER_TOKEN: &str = "leave-without-membership-stranger-access-token";

/// A leave from a user with nothing to leave records no departure.
///
/// A stranger must not get the room as a left room, and a kicked member's
/// departure, which bounds the history it may read, must stay at the kick.
#[test]
fn leave_without_membership_records_no_departure() -> Result {
	let options: [&str; 0] = [];

	boot("leave-without-membership", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "leaveowner", OWNER_TOKEN).await?;

	let member_id = register(services, "leavemember", MEMBER_TOKEN).await?;
	let stranger_id = register(services, "leavestranger", STRANGER_TOKEN).await?;

	let owner = Client { services, base, token: OWNER_TOKEN };
	let member = Client { services, base, token: MEMBER_TOKEN };
	let stranger = Client { services, base, token: STRANGER_TOKEN };

	let room_id = owner
		.create_room(&json!({ "preset": "private_chat" }))
		.await?;

	let path = |action| format!("rooms/{room_id}/{action}");
	let target = json!({ "user_id": member_id });

	owner.post(&path("invite"), &target).await?;
	member.post(&path("join"), &json!({})).await?;
	owner.post(&path("kick"), &target).await?;

	let state_cache = &services.state_cache;
	let kicked_at = state_cache
		.get_left_count(&room_id, &member_id)
		.await?;

	stranger.post(&path("leave"), &json!({})).await?;
	member.post(&path("leave"), &json!({})).await?;

	let left_at = state_cache
		.get_left_count(&room_id, &member_id)
		.await?;

	assert!(!state_cache.is_left(&stranger_id, &room_id).await);
	assert_eq!(left_at, kicked_at);

	Ok(())
}
