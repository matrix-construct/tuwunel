#![cfg(test)]

use serde_json::json;
use tuwunel_core::{Result, err, ruma::RoomId};
use tuwunel_service::Services;

use self::{
	appservice::{Bridge, register_appservice},
	client::{Client, register},
	fixture::boot,
};

mod appservice;
mod client;
mod fixture;

const ADMIN_TOKEN: &str = "room-alias-creation-policy-admin-token";
const USER_TOKEN: &str = "room-alias-creation-policy-user-token";
const BRIDGE_TOKEN: &str = "room-alias-creation-policy-bridge-token";

const BRIDGE: Bridge<'static> = Bridge {
	id: "room-alias-creation-policy",
	token: BRIDGE_TOKEN,
	sender_localpart: "alias_bridge",
	users: "^@alias_bridge:.*$",
	aliases: Some("^#bridge_alias:.*$"),
};

#[test]
fn alias_creation_preserves_admin_and_appservice_access() -> Result {
	boot("room-alias-creation-policy", ["allow_room_alias_creation=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let admin_id = register(services, "alias_policy_admin", ADMIN_TOKEN).await?;

	register(services, "alias_policy_user", USER_TOKEN).await?;
	services.admin.make_user_admin(&admin_id).await?;

	let admin = Client { services, base, token: ADMIN_TOKEN };
	let user = Client { services, base, token: USER_TOKEN };
	let room_id = admin
		.create_room(&json!({ "preset": "private_chat" }))
		.await?;

	let server_name = services.globals.server_name();

	assert_status(create_alias(&user, &format!("ordinary:{server_name}"), &room_id).await?, 403)?;
	assert_status(create_alias(&admin, &format!("admin:{server_name}"), &room_id).await?, 200)?;
	assert_status(create_room_with_alias(&user, "ordinary_room").await?, 403)?;
	assert_status(create_room_with_alias(&admin, "admin_room").await?, 200)?;

	register_appservice(services, &BRIDGE).await?;

	let bridge = Client { services, base, token: BRIDGE_TOKEN };

	assert_status(
		create_alias(&bridge, &format!("bridge_alias:{server_name}"), &room_id).await?,
		200,
	)
}

async fn create_alias(client: &Client<'_>, alias: &str, room_id: &RoomId) -> Result<u16> {
	Ok(client
		.services
		.client
		.clients
		.default
		.put(client.url(&format!("directory/room/%23{alias}")))
		.bearer_auth(client.token)
		.json(&json!({ "room_id": room_id }))
		.send()
		.await?
		.status()
		.as_u16())
}

fn assert_status(status: u16, expected: u16) -> Result {
	(status == expected)
		.then_some(())
		.ok_or_else(|| err!("status was {status}, expected {expected}"))
}

async fn create_room_with_alias(client: &Client<'_>, alias_name: &str) -> Result<u16> {
	Ok(client
		.post_url(&client.url("createRoom"), &json!({ "room_alias_name": alias_name }))
		.await?
		.status()
		.as_u16())
}
