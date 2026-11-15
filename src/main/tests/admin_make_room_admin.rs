#![cfg(test)]

use std::time::Duration;

use reqwest::Method;
use serde_json::json;
use tokio::time::timeout;
use tuwunel_core::{
	Result, err,
	ruma::{
		Int, OwnedEventId, RoomId, UserId,
		events::{
			StateEventType,
			room::{member::MembershipState, power_levels::RoomPowerLevelsEventContent},
		},
	},
};
use tuwunel_matrix::Event;
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

mod client;
mod fixture;

#[path = "client/admin_api.rs"]
mod admin_api;

const ADMIN_TOKEN: &str = "admin-make-room-admin-admin-access-token";
const OWNER_TOKEN: &str = "admin-make-room-admin-owner-access-token";
const TARGET_TOKEN: &str = "admin-make-room-admin-target-access-token";

const GRANT_DEADLINE: Duration = Duration::from_secs(10);

/// The Synapse room admin grant invites a user holding no membership in a
/// private room, then raises their power.
///
/// The server admin making the request is not in the room; the room's
/// highest-powered local member sends both events. A second grant finds the
/// user invited and sends no second invite.
#[test]
fn grants_room_admin_to_a_non_member() -> Result {
	let options = ["admin.room.create=true", "registration.grant_admin_to_first_user=false"];

	boot("admin-make-room-admin", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let admin_id = register(services, "grantadmin", ADMIN_TOKEN).await?;
	let owner_id = register(services, "grantowner", OWNER_TOKEN).await?;
	let target_id = register(services, "granttarget", TARGET_TOKEN).await?;

	services.admin.make_user_admin(&admin_id).await?;

	let admin = Client { services, base, token: ADMIN_TOKEN };
	let owner = Client { services, base, token: OWNER_TOKEN };
	let private = json!({ "preset": "private_chat" });
	let room_id = owner.create_room(&private).await?;

	let membership = async || {
		services
			.state_cache
			.user_membership(&target_id, &room_id)
			.await
	};

	assert_eq!(membership().await, None);

	let grants = async || -> Result<OwnedEventId> {
		grant(&admin, &room_id, &target_id).await?;

		let power_levels = services
			.state_accessor
			.room_state_get(&room_id, &StateEventType::RoomPowerLevels, "")
			.await?;

		let member = services
			.state_accessor
			.room_state_get(&room_id, &StateEventType::RoomMember, target_id.as_str())
			.await?;

		let levels: RoomPowerLevelsEventContent = power_levels.get_content()?;

		assert_eq!(membership().await, Some(MembershipState::Invite));
		assert_eq!(levels.users.get(&target_id), Some(&Int::from(100_u8)));
		assert_eq!(power_levels.sender, owner_id);
		assert_eq!(member.sender, owner_id);

		Ok(member.event_id)
	};

	let invite = grants().await?;

	assert_eq!(grants().await?, invite);

	Ok(())
}

/// Request the room admin grant for `user_id`, requiring a 200 within the
/// deadline.
///
/// A grant that never answers fails the case here instead of hanging it.
async fn grant(client: &Client<'_>, room_id: &RoomId, user_id: &UserId) -> Result {
	let path = format!("v1/rooms/{room_id}/make_room_admin");
	let body = json!({ "user_id": user_id });
	let request = client.admin_api(Method::POST, &path, &body, 200);

	timeout(GRANT_DEADLINE, request)
		.await
		.map_err(|_| err!("make_room_admin did not answer within {GRANT_DEADLINE:?}"))?
		.map(drop)
}
