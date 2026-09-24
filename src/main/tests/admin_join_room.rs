#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{RoomId, UserId, events::room::member::MembershipState},
};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

mod client;
mod fixture;

const ADMIN_TOKEN: &str = "admin-join-room-admin-access-token";
const OWNER_TOKEN: &str = "admin-join-room-owner-access-token";
const USER_TOKEN: &str = "admin-join-room-user-access-token";

/// Drives the Synapse admin join through the HTTP router.
///
/// A room that would refuse the user uninvited admits the join only after
/// the admin invites them, so the admin's membership and power decide the
/// outcome there. A public room, or a restricted one whose allow rule the
/// user already meets, admits the join without the admin present at all.
#[test]
fn invites_only_where_the_join_rule_requires_it() -> Result {
	let options = ["create_admin_room=true", "grant_admin_to_first_user=false"];

	boot("admin-join-room", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let admin_id = register(services, "joinadmin", ADMIN_TOKEN).await?;
	let user_id = register(services, "joinuser", USER_TOKEN).await?;

	register(services, "joinowner", OWNER_TOKEN).await?;
	services.admin.make_user_admin(&admin_id).await?;

	let admin = Client { services, base, token: ADMIN_TOKEN };
	let owner = Client { services, base, token: OWNER_TOKEN };
	let private = json!({ "preset": "private_chat" });
	let public = json!({ "preset": "public_chat" });
	let admins_room = admin.create_room(&private).await?;
	let owners_room = owner.create_room(&private).await?;
	let public_room = owner.create_room(&public).await?;
	let met = restricted(&public_room);
	let unmet = restricted(&owners_room);
	let met_room = owner.create_room(&met).await?;
	let unmet_room = admin.create_room(&unmet).await?;
	let weak = json!({
		"preset": "private_chat",
		"invite": [&admin_id],
		"power_level_content_override": { "invite": 100 },
	});

	let weak_room = owner.create_room(&weak).await?;
	let join = format!("rooms/{weak_room}/join");

	admin.post(&join, &json!({})).await?;

	let membership = async |room_id: &RoomId| {
		services
			.state_cache
			.user_membership(&user_id, room_id)
			.await
	};

	let joins = async |room_id: &RoomId| -> Result {
		admin_join(&admin, room_id, &user_id, 200).await?;
		assert_eq!(membership(room_id).await, Some(MembershipState::Join));

		Ok(())
	};

	let refuses = async |room_id: &RoomId| -> Result {
		let refusal = admin_join(&admin, room_id, &user_id, 403).await?;

		assert_eq!(refusal["errcode"], "M_FORBIDDEN");
		assert_eq!(membership(room_id).await, None);

		Ok(())
	};

	joins(&admins_room).await?;
	joins(&admins_room).await?;
	refuses(&owners_room).await?;
	joins(&public_room).await?;
	joins(&met_room).await?;
	joins(&unmet_room).await?;
	refuses(&weak_room).await
}

/// A restricted room's `createRoom` body admitting `allowed`'s members.
///
/// The initial join rule overrides the `private_chat` preset's, so only
/// membership of the allowed room lets a user in without an invite.
fn restricted(allowed: &RoomId) -> Value {
	json!({
		"preset": "private_chat",
		"initial_state": [{
			"type": "m.room.join_rules",
			"state_key": "",
			"content": {
				"join_rule": "restricted",
				"allow": [{ "type": "m.room_membership", "room_id": allowed }],
			},
		}],
	})
}

/// Posts the admin join of `user_id` to `room_id`, asserts the `expected`
/// status, and returns the parsed body.
///
/// The status assertion carries the response body, so a routing or fixture
/// failure reads differently from the refusal a case expects.
async fn admin_join(
	client: &Client<'_>,
	room_id: &RoomId,
	user_id: &UserId,
	expected: u16,
) -> Result<Value> {
	let url = format!("{}/_synapse/admin/v1/join/{room_id}", client.base);
	let body = json!({ "user_id": user_id });
	let response = client.post_url(&url, &body).await?;
	let status = response.status().as_u16();
	let response = response.text().await?;

	assert_eq!(status, expected, "{url}: {response}");

	Ok(response.parse()?)
}
