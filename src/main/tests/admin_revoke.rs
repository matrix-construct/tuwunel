#![cfg(test)]

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{
		UserId,
		events::room::{
			member::{MembershipState, RoomMemberEventContent},
			message::RoomMessageEventContent,
		},
		profile::ProfileFieldName,
	},
};
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::{Services, profile::MAX_DISPLAYNAME_LENGTH, users::Register};

use self::{
	admin::{accepted, refused},
	client::{Client, poll_until, register},
	fixture::boot,
	timeline::{append_message, append_pdu},
};

mod admin;
#[expect(dead_code)] // Only the user admin and MAS APIs are requested through the client harness.
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
const MAS_SECRET: &str = "admin-revoke-mas-secret";

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
	let options = ["admin.room.create=true", "registration.grant_admin_to_first_user=false"];

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
/// A malformed email or a refused profile value refuses the whole request,
/// from the admin API or MAS, before any of its fields is written.
#[test]
fn put_user_leaves_a_non_admin_alone() -> Result {
	let options = [
		"admin.room.create=true",
		"registration.grant_admin_to_first_user=false",
		"logging.enable=false",
		&format!("authentication.mas_secret=\"{MAS_SECRET}\""),
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

	new_account_profiles(&admin).await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn new_account_profiles(admin: &Client<'_>) -> Result {
	let services = admin.services;
	let server_name = services.globals.server_name();
	let long_name = "n".repeat(MAX_DISPLAYNAME_LENGTH + 44);
	let shortened = UserId::parse_with_server_name("shortened", server_name)?;

	services
		.users
		.full_register(Register {
			user_id: Some(&shortened),
			password: Some("shortened-password"),
			displayname: Some(&long_name),
			..Default::default()
		})
		.await?;

	let stored = services.profile.displayname(&shortened).await?;

	assert_eq!(stored, "n".repeat(MAX_DISPLAYNAME_LENGTH));

	let absent = UserId::parse_with_server_name("absent", server_name)?;
	let long = json!({ "displayname": long_name });
	let invalid = json!({ "avatar_url": oversized_avatar() });

	for body in [&long, &invalid] {
		put_user(admin, &absent, body, 400).await?;

		assert!(!services.users.exists(&absent).await, "a refused profile left an account");
	}

	existing_account_profile(admin, &shortened, &long_name).await?;
	combined_profile_size(admin).await?;
	provisioned_profiles(admin).await
}

// One byte past the profile's 1000-byte avatar cap.
fn oversized_avatar() -> String { format!("mxc://localhost/{}", "a".repeat(985)) }

#[tracing::instrument(level = "debug", skip_all)]
async fn existing_account_profile(
	admin: &Client<'_>,
	user_id: &UserId,
	long_name: &str,
) -> Result {
	let password = || admin.services.users.password_hash(user_id);
	let before = password().await?;
	let body = json!({
		"password": "replaced-password",
		"logout_devices": true,
		"displayname": long_name,
	});

	put_user(admin, user_id, &body, 400).await?;

	assert_eq!(password().await?, before, "a refused profile changed the password");

	Ok(())
}

/// One request's name and avatar are sized together against the profile cap.
///
/// The padding leaves exactly enough room for a 256-byte name, so a name and an
/// avatar added together are refused, while clearing the name makes room for the
/// avatar in the same request.
#[tracing::instrument(level = "debug", skip_all)]
async fn combined_profile_size(admin: &Client<'_>) -> Result {
	let services = admin.services;
	let padded = UserId::parse_with_server_name("padded", services.globals.server_name())?;
	let created = json!({ "password": "padded-password" });

	put_user(admin, &padded, &created, 200).await?;

	// 65241 bytes of padding and a 256-byte name serialize to exactly 65536.
	let pad = [(ProfileFieldName::from("com.example.pad"), Some(json!("p".repeat(65_241))))];

	services
		.profile
		.set_profile_keys(&padded, &pad, None)
		.await?;

	let name = "n".repeat(MAX_DISPLAYNAME_LENGTH);
	let avatar = format!("mxc://localhost/{}", "a".repeat(84));
	let both = json!({ "displayname": name, "avatar_url": avatar });

	put_user(admin, &padded, &both, 400).await?;

	assert!(
		services
			.profile
			.displayname(&padded)
			.await
			.is_err(),
		"a refused write set a name"
	);

	put_user(admin, &padded, &json!({ "displayname": name }), 200).await?;

	let swapped = json!({ "displayname": "", "avatar_url": avatar });

	put_user(admin, &padded, &swapped, 200).await?;

	assert!(
		services
			.profile
			.displayname(&padded)
			.await
			.is_err(),
		"the name was not cleared"
	);

	assert_eq!(
		services
			.profile
			.avatar_url(&padded)
			.await?
			.as_str(),
		avatar
	);

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn provisioned_profiles(admin: &Client<'_>) -> Result {
	let services = admin.services;
	let server_name = services.globals.server_name();
	let mas = Client {
		services,
		base: admin.base,
		token: MAS_SECRET,
	};

	let url = format!("{}/_synapse/mas/provision_user", admin.base);

	// Two bytes a character, so a cut by bytes would split one or keep too few.
	let long_name = "\u{e9}".repeat(MAX_DISPLAYNAME_LENGTH + 44);
	let provisioned = UserId::parse_with_server_name("provisioned", server_name)?;
	let body = json!({
		"localpart": provisioned.localpart(),
		"set_displayname": long_name,
		"locked": true,
	});

	assert_eq!(mas.post_url(&url, &body).await?.status(), StatusCode::CREATED);

	let stored = services.profile.displayname(&provisioned).await?;

	assert_eq!(stored, "\u{e9}".repeat(MAX_DISPLAYNAME_LENGTH));
	assert!(services.users.is_locked(&provisioned).await, "the account was left unlocked");

	let unprovisioned = UserId::parse_with_server_name("unprovisioned", server_name)?;
	let invalid = json!({
		"localpart": unprovisioned.localpart(),
		"set_avatar_url": oversized_avatar(),
	});

	assert_eq!(mas.post_url(&url, &invalid).await?.status(), StatusCode::BAD_REQUEST);
	assert!(
		!services.users.exists(&unprovisioned).await,
		"a refused profile left an account"
	);

	Ok(())
}

async fn put_user(client: &Client<'_>, user_id: &UserId, body: &Value, expected: u16) -> Result {
	let path = format!("v2/users/{user_id}");

	client
		.admin_api(Method::PUT, &path, body, expected)
		.await
		.map(drop)
}
