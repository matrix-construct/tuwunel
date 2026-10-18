#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{
	Result, err,
	ruma::{OwnedUserId, RoomId, UserId},
};
use tuwunel_service::{Services, users::Register};

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

const OWNER_TOKEN: &str = "appservice-user-join-owner-token";
const GUEST_TOKEN: &str = "appservice-user-join-guest-token";
const BRIDGE_TOKEN: &str = "appservice-user-join-bridge-token";

const BRIDGE: Bridge<'static> = Bridge {
	id: "appservice-user-join",
	token: BRIDGE_TOKEN,
	sender_localpart: "join_bridge",
	users: "^@join_bot:.*$",
	aliases: None,
};

/// A passwordless user an appservice registered is not a guest.
///
/// It shares the empty password marker with guests, yet with its own access
/// token it joins a room that forbids guests, and whoami does not call it a
/// guest. A guest invited to the same room is still refused and still reported
/// as one.
#[test]
fn appservice_users_are_not_guests() -> Result {
	boot("appservice-user-join", ["grant_admin_to_first_user=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "join_owner", OWNER_TOKEN).await?;
	register_appservice(services, &BRIDGE).await?;

	let guest_id = register_guest(services, "join_guest", GUEST_TOKEN).await?;
	let bridge = Client { services, base, token: BRIDGE_TOKEN };
	let registered = bridge
		.post(
			"register",
			&json!({ "type": "m.login.application_service", "username": "join_bot" }),
		)
		.await?;

	let bot_id: OwnedUserId = field(&registered, "user_id")?.try_into()?;
	let bot_token = field(&registered, "access_token")?;
	let bot = Client { services, base, token: bot_token };
	let guest = Client { services, base, token: GUEST_TOKEN };
	let owner = Client { services, base, token: OWNER_TOKEN };

	assert!(
		services.users.is_deactivated(&bot_id).await?,
		"the appservice user has no password, like a guest"
	);

	let room_id = owner
		.create_room(&json!({
			"preset": "private_chat",
			"invite": [&bot_id, &guest_id],
			"initial_state": [{
				"type": "m.room.guest_access",
				"state_key": "",
				"content": { "guest_access": "forbidden" },
			}],
		}))
		.await?;

	expect_status(join(&guest, &room_id).await?, 403)?;
	expect_status(join(&bot, &room_id).await?, 200)?;

	assert!(!is_guest(&bot).await?, "whoami calls the appservice user a guest");
	assert!(is_guest(&guest).await?, "whoami no longer reports the guest");
	assert!(!services.users.is_guest(&bot_id).await);
	assert!(services.users.is_guest(&guest_id).await);

	Ok(())
}

/// Register a guest account and give it a device holding `token`.
///
/// The account takes the guest registration path, so it stores the same empty
/// password as the appservice user it is compared with.
async fn register_guest(
	services: &Services,
	localpart: &str,
	token: &str,
) -> Result<OwnedUserId> {
	let user_id = UserId::parse_with_server_name(localpart, services.globals.server_name())?;

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			is_guest: true,
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	Ok(user_id)
}

async fn join(client: &Client<'_>, room_id: &RoomId) -> Result<u16> {
	let url = client.url(&format!("rooms/{room_id}/join"));
	let response = client.post_url(&url, &json!({})).await?;

	Ok(response.status().as_u16())
}

fn expect_status(status: u16, expected: u16) -> Result {
	status
		.eq(&expected)
		.then_some(())
		.ok_or_else(|| err!("status was {status}, expected {expected}"))
}

async fn is_guest(client: &Client<'_>) -> Result<bool> {
	let whoami = client.get("account/whoami").await?;

	// The field is left out when false.
	let is_guest = whoami
		.get("is_guest")
		.and_then(Value::as_bool)
		.unwrap_or(false);

	Ok(is_guest)
}
