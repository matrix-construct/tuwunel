#![cfg(test)]

use reqwest::StatusCode;
use serde_json::json;
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, register},
	fixture::boot,
};

mod client;
mod fixture;

#[path = "client/get.rs"]
mod get;

#[path = "client/put.rs"]
mod put;

#[path = "client/summary.rs"]
mod summary;

const OWNER_TOKEN: &str = "state-departed-member-owner-access-token";
const MEMBER_TOKEN: &str = "state-departed-member-member-access-token";
const NEWCOMER_TOKEN: &str = "state-departed-member-newcomer-access-token";

/// A former member is refused the room summary, which shows current state.
///
/// After the member is kicked, the room is renamed and another user invited.
/// The kicked member no longer sees the room as it is now, while the owner
/// reads the new name and the invited newcomer previews the room.
#[test]
fn departed_member_is_refused_current_state() -> Result {
	let options: [&str; 0] = [];

	boot("state-departed-member", options, exercise)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "stateowner", OWNER_TOKEN).await?;

	let member_id = register(services, "statemember", MEMBER_TOKEN).await?;
	let newcomer_id = register(services, "statenewcomer", NEWCOMER_TOKEN).await?;

	let owner = Client { services, base, token: OWNER_TOKEN };
	let member = Client { services, base, token: MEMBER_TOKEN };
	let newcomer = Client { services, base, token: NEWCOMER_TOKEN };

	let room_id = owner
		.create_room(&json!({ "preset": "private_chat", "name": "before" }))
		.await?;

	let path = |action| format!("rooms/{room_id}/{action}");
	let target = json!({ "user_id": member_id });
	let invitee = json!({ "user_id": newcomer_id });

	owner.post(&path("invite"), &target).await?;
	member.post(&path("join"), &json!({})).await?;
	owner.post(&path("kick"), &target).await?;

	owner
		.put(&path("state/m.room.name/"), &json!({ "name": "after" }))
		.await?;

	owner.post(&path("invite"), &invitee).await?;

	let member_summary = member.summary(&room_id).await?;
	let owner_summary = accepted(owner.summary(&room_id).await?).await?;
	let newcomer_summary = accepted(newcomer.summary(&room_id).await?).await?;

	assert_eq!(
		member_summary.status(),
		StatusCode::FORBIDDEN,
		"kicked member reads the current summary"
	);

	assert_eq!(
		owner_summary["name"], "after",
		"owner's summary lacks the new name: {owner_summary}"
	);

	assert_eq!(
		newcomer_summary["membership"], "invite",
		"invited newcomer is refused the summary: {newcomer_summary}"
	);

	Ok(())
}
