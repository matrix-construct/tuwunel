#![cfg(test)]

use serde_json::json;
use tuwunel_core::{Result, err, ruma::RoomId};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

mod client;
mod fixture;

const ADMIN_TOKEN: &str = "room-alias-creation-policy-admin-token";
const USER_TOKEN: &str = "room-alias-creation-policy-user-token";
const BRIDGE_TOKEN: &str = "room-alias-creation-policy-bridge-token";

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

	assert_status(
		create_alias(&user, &format!("ordinary:{server_name}"), &room_id).await?,
		403,
	)?;
	assert_status(
		create_alias(&admin, &format!("admin:{server_name}"), &room_id).await?,
		200,
	)?;

	register_appservice(services).await?;
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

async fn register_appservice(services: &Services) -> Result {
	let registration = json!({
		"id": "room-alias-creation-policy",
		"url": null,
		"as_token": BRIDGE_TOKEN,
		"hs_token": "room-alias-creation-policy-hs-token",
		"sender_localpart": "alias_bridge",
		"namespaces": {
			"users": [{"exclusive": true, "regex": "^@alias_bridge:.*$"}],
			"aliases": [{"exclusive": true, "regex": "^#bridge_alias:.*$"}],
			"rooms": []
		}
	});

	services
		.appservice
		.register_appservice(serde_json::from_value(registration)?)
		.await
}
