#![cfg(test)]

use std::time::Duration;

use tuwunel_core::{
	Result,
	ruma::{
		UserId,
		events::room::{
			member::{MembershipState, RoomMemberEventContent},
			message::RoomMessageEventContent,
		},
	},
};
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::Services;

use self::{
	admin::{accepted, refused},
	client::{poll_until, register},
	fixture::boot,
	timeline::{append_message, append_pdu},
};

mod admin;
#[expect(dead_code)] // This test sends no client requests of its own.
mod client;
mod fixture;
mod timeline;

const REVOKER_TOKEN: &str = "admin-revoke-revoker-access-token";
const OTHER_TOKEN: &str = "admin-revoke-other-admin-access-token";
const TARGET_TOKEN: &str = "admin-revoke-target-admin-access-token";

const COMMAND_DEADLINE: Duration = Duration::from_secs(10);

/// `revoke-admin` takes a user out of the admin room, but never the server
/// user or the admin sending it.
///
/// A name with no account loses its grant without gaining an account, and a
/// revoked admin can be granted again. The worker runs admin-room commands in
/// arrival order, so once a second admin's revocation of a third has run, the
/// first admin's earlier revocation of themselves has been refused or has
/// taken effect.
#[test]
fn revoke_admin_spares_the_server_user_and_the_sender() -> Result {
	let options = ["create_admin_room=true", "grant_admin_to_first_user=false"];

	boot("admin-revoke", options, exercise)
}

async fn exercise(services: &Services, _: &str) -> Result {
	let revoker_id = register(services, "revoker", REVOKER_TOKEN).await?;
	let other_id = register(services, "other", OTHER_TOKEN).await?;
	let target_id = register(services, "target", TARGET_TOKEN).await?;
	let ghost_id = UserId::parse_with_server_name("ghost", services.globals.server_name())?;

	for user_id in [&revoker_id, &other_id, &target_id] {
		services.admin.make_user_admin(user_id).await?;
	}

	join_admin_room(services, &ghost_id).await?;

	assert!(services.admin.user_is_admin(&ghost_id).await);

	accepted(services, "users revoke-admin ghost").await?;

	assert!(!services.admin.user_is_admin(&ghost_id).await);
	assert!(!services.users.exists(&ghost_id).await);

	let server_user = &services.globals.server_user;

	refused(services, &format!("users revoke-admin {server_user}"), "cannot be revoked").await?;

	let admin_room = services.admin.get_admin_room().await?;
	let command = |target: &str| {
		RoomMessageEventContent::text_plain(format!("!admin users revoke-admin {target}"))
	};

	append_message(services, &revoker_id, &admin_room, &command("revoker")).await?;
	append_message(services, &other_id, &admin_room, &command("target")).await?;

	let revoked = async || !services.admin.user_is_admin(&target_id).await;

	assert!(poll_until(COMMAND_DEADLINE, revoked).await, "revoking another admin never ran");
	assert!(services.admin.user_is_admin(&revoker_id).await, "an admin revoked themselves");

	accepted(services, "users make-user-admin target").await?;

	assert!(services.admin.user_is_admin(&target_id).await);

	Ok(())
}

/// Join a name with no account to the admin room.
///
/// This is the membership an admin grant left behind when a grant did not yet
/// require an account.
async fn join_admin_room(services: &Services, user_id: &UserId) -> Result {
	let admin_room = services.admin.get_admin_room().await?;
	let server_user = &services.globals.server_user;
	let member_event = |membership| {
		PduBuilder::state(user_id.as_str(), &RoomMemberEventContent::new(membership))
	};

	let invite = member_event(MembershipState::Invite);
	let join = member_event(MembershipState::Join);

	append_pdu(services, server_user, &admin_room, invite).await?;
	append_pdu(services, user_id, &admin_room, join)
		.await
		.map(drop)
}
