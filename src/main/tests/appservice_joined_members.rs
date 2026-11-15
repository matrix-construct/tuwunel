#![cfg(test)]

use serde_json::json;
use tuwunel_core::{
	Result, err,
	ruma::{OwnedUserId, RoomId},
};
use tuwunel_service::Services;

use self::{
	appservice::{Bridge, register_appservice},
	client::{Client, field, register},
	fixture::boot,
};

mod appservice;
mod client;
mod fixture;

#[path = "client/get.rs"]
mod get;

const OWNER_TOKEN: &str = "appservice-joined-members-owner-token";
const OUTSIDER_TOKEN: &str = "appservice-joined-members-outsider-token";
const BRIDGE_TOKEN: &str = "appservice-joined-members-bridge-token";

const BRIDGE: Bridge<'static> = Bridge {
	id: "appservice-joined-members",
	token: BRIDGE_TOKEN,
	sender_localpart: "members_bridge",
	users: "^@members_bot:.*$",
	aliases: None,
};

/// An appservice lists a room's joined members through one of its users.
///
/// Its own sender user never joins the room. The request on the appservice
/// token is refused while none of its namespaced users is joined, and is
/// served once one is. An ordinary user outside the room stays refused.
#[test]
fn appservice_lists_joined_members_through_its_user() -> Result {
	boot(
		"appservice-joined-members",
		["registration.grant_admin_to_first_user=false"],
		exercise,
	)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let owner_id = register(services, "members_owner", OWNER_TOKEN).await?;
	register(services, "members_outsider", OUTSIDER_TOKEN).await?;
	register_appservice(services, &BRIDGE).await?;

	let bridge = Client { services, base, token: BRIDGE_TOKEN };
	let registered = bridge
		.post(
			"register",
			&json!({ "type": "m.login.application_service", "username": "members_bot" }),
		)
		.await?;

	let bot_id: OwnedUserId = field(&registered, "user_id")?.try_into()?;
	let bot_token = field(&registered, "access_token")?;
	let bot = Client { services, base, token: bot_token };
	let owner = Client { services, base, token: OWNER_TOKEN };
	let outsider = Client { services, base, token: OUTSIDER_TOKEN };

	let room_id = owner
		.create_room(&json!({ "preset": "private_chat", "invite": [&bot_id] }))
		.await?;

	// The bot is only invited, so the appservice has no user in the room yet.
	expect_status(joined_members(&bridge, &room_id).await?, 403)?;

	let url = bot.url(&format!("rooms/{room_id}/join"));
	let joined = bot.post_url(&url, &json!({})).await?;
	expect_status(joined.status().as_u16(), 200)?;

	let members = bridge
		.get(&format!("rooms/{room_id}/joined_members"))
		.await?;

	let joined = members
		.get("joined")
		.and_then(|joined| joined.as_object())
		.ok_or_else(|| err!("the reply carries no `joined` object: {members}"))?;

	assert!(joined.contains_key(owner_id.as_str()), "the owner is missing: {members}");
	assert!(joined.contains_key(bot_id.as_str()), "the bot is missing: {members}");
	assert_eq!(joined.len(), 2, "unexpected members: {members}");

	expect_status(joined_members(&outsider, &room_id).await?, 403)?;

	Ok(())
}

async fn joined_members(client: &Client<'_>, room_id: &RoomId) -> Result<u16> {
	let url = client.url(&format!("rooms/{room_id}/joined_members"));
	let response = client.get_url(&url).await?;

	Ok(response.status().as_u16())
}

fn expect_status(status: u16, expected: u16) -> Result {
	status
		.eq(&expected)
		.then_some(())
		.ok_or_else(|| err!("status was {status}, expected {expected}"))
}
