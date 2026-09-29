#![cfg(test)]

use serde_json::json;
use tuwunel_core::{
	Result,
	ruma::{
		UserId,
		events::{StateEventType, room::power_levels::RoomPowerLevelsEventContent},
	},
};
use tuwunel_service::Services;

use self::{
	admin::{accepted, refused},
	client::{Client, register},
	fixture::boot,
};

mod admin;
mod client;
mod fixture;

const ADMIN_TOKEN: &str = "admin-absent-user-admin-access-token";
const GHOST_TOKEN: &str = "admin-absent-user-ghost-access-token";

const ABSENT: &str = "does not exist on this server";

/// User commands, and the admin grant beneath them, refuse a local user who
/// holds no account.
///
/// Each would otherwise create the account, or stage an admin grant, a room
/// membership or a power level for whoever registers the name later. Once
/// every command has been refused, registering the name finds nothing waiting
/// for it, while an existing account without a password, the server user's,
/// is still joined to a room admitting guests.
#[test]
fn user_commands_refuse_an_absent_user() -> Result {
	let options = ["create_admin_room=true", "grant_admin_to_first_user=false"];

	boot("admin-absent-user", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let admin_id = register(services, "absentadmin", ADMIN_TOKEN).await?;

	services.admin.make_user_admin(&admin_id).await?;

	let admin = Client { services, base, token: ADMIN_TOKEN };
	let public = json!({
		"preset": "public_chat",
		"initial_state": [{
			"type": "m.room.guest_access",
			"state_key": "",
			"content": { "guest_access": "can_join" },
		}],
	});

	let public_room = admin.create_room(&public).await?;
	let server_user = &services.globals.server_user;

	refused(services, "users make-user-admin ghost", ABSENT).await?;
	refused(services, &format!("users force-join-room ghost {public_room}"), ABSENT).await?;
	refused(services, &format!("users force-promote ghost {public_room}"), ABSENT).await?;
	refused(services, "users reset-password ghost not-a-real-password", ABSENT).await?;
	refused(services, "users deactivate ghost", ABSENT).await?;
	accepted(services, &format!("users force-join-room {server_user} {public_room}")).await?;

	let ghost_id = UserId::parse_with_server_name("ghost", services.globals.server_name())?;
	let grant = services.admin.make_user_admin(&ghost_id).await;

	assert!(grant.is_err_and(|error| error.is_not_found() && error.to_string().contains(ABSENT)));

	register(services, ghost_id.localpart(), GHOST_TOKEN).await?;

	assert!(!services.admin.user_is_admin(&ghost_id).await);

	let power_levels: RoomPowerLevelsEventContent = services
		.state_accessor
		.room_state_get_content(&public_room, &StateEventType::RoomPowerLevels, "")
		.await?;

	assert!(!power_levels.users.contains_key(&ghost_id));

	Ok(())
}
