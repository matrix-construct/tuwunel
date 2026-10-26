#![cfg(test)]

use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};
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
	client::{Client, poll_until, register},
	fixture::boot,
	timeline::{append_message, append_pdu},
};

mod admin;
#[expect(dead_code)] // Only the user admin API is requested through the client harness.
mod client;
mod fixture;
mod timeline;

#[path = "client/admin_api.rs"]
mod admin_api;

const REVOKER_TOKEN: &str = "admin-revoke-revoker-access-token";
const OTHER_TOKEN: &str = "admin-revoke-other-admin-access-token";
const TARGET_TOKEN: &str = "admin-revoke-target-admin-access-token";
const ADMIN_TOKEN: &str = "admin-revoke-put-admin-access-token";
const MEMBER_TOKEN: &str = "admin-revoke-put-member-access-token";
const PENDING_TOKEN: &str = "admin-revoke-put-pending-access-token";

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
	let options = ["create_admin_room=true", "registration.grant_admin_to_first_user=false"];

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

	refused(services, "users revoke-admin ghost", "is not an admin").await?;

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

/// `admin: false` in the user admin API leaves a non-admin as they are, and
/// the rest of the request still applies.
///
/// That holds for an account never granted, one already demoted, and one the
/// request creates, while a pending invite to the admin room is still revoked.
/// A malformed email refuses the whole request before any of its fields is
/// written.
#[test]
fn put_user_leaves_a_non_admin_alone() -> Result {
	let options = [
		"create_admin_room=true",
		"registration.grant_admin_to_first_user=false",
		"log_enable=false",
	];

	boot("admin-revoke-put-user", options, put_user_requests)
}

async fn put_user_requests(services: &Services, base: &str) -> Result {
	let admin_id = register(services, "admin", ADMIN_TOKEN).await?;
	let member_id = register(services, "member", MEMBER_TOKEN).await?;
	let newcomer_id = UserId::parse_with_server_name("newcomer", services.globals.server_name())?;
	let admin = Client { services, base, token: ADMIN_TOKEN };

	services.admin.make_user_admin(&admin_id).await?;

	let admin_room = services.admin.get_admin_room().await?;
	let renamed = json!({ "displayname": "Renamed", "admin": false });

	put_user(&admin, &member_id, &renamed, 200).await?;

	let member = services
		.state_accessor
		.get_member(&admin_room, &member_id)
		.await;

	assert!(member.is_err_and(|e| e.is_missing()), "a no-op demotion wrote a membership");
	assert!(!services.admin.user_is_admin(&member_id).await);
	assert_eq!(services.profile.displayname(&member_id).await?, "Renamed");

	services.admin.make_user_admin(&member_id).await?;

	let demoted = json!({ "admin": false });

	put_user(&admin, &member_id, &demoted, 200).await?;

	assert!(!services.admin.user_is_admin(&member_id).await);

	put_user(&admin, &member_id, &demoted, 200).await?;

	let created = json!({ "password": "newcomer-password", "admin": false });

	put_user(&admin, &newcomer_id, &created, 200).await?;

	assert!(services.users.exists(&newcomer_id).await);
	assert!(!services.admin.user_is_admin(&newcomer_id).await);

	let pending_id = register(services, "pending", PENDING_TOKEN).await?;
	let server_user = &services.globals.server_user;
	let invite = RoomMemberEventContent::new(MembershipState::Invite);

	append_pdu(
		services,
		server_user,
		&admin_room,
		PduBuilder::state(pending_id.as_str(), &invite),
	)
	.await?;

	let invited = async || {
		services
			.state_cache
			.is_invited(&pending_id, &admin_room)
			.await
	};

	assert!(invited().await, "the staged invite never landed");

	put_user(&admin, &pending_id, &demoted, 200).await?;

	assert!(!invited().await, "a pending invite survived the demotion");

	let malformed = json!({
		"displayname": "Unapplied",
		"threepids": [{ "medium": "email", "address": "no-domain" }],
	});

	put_user(&admin, &member_id, &malformed, 400).await?;

	assert_eq!(services.profile.displayname(&member_id).await?, "Renamed");

	Ok(())
}

async fn put_user(client: &Client<'_>, user_id: &UserId, body: &Value, expected: u16) -> Result {
	let path = format!("v2/users/{user_id}");

	client
		.admin_api(Method::PUT, &path, body, expected)
		.await
		.map(drop)
}
