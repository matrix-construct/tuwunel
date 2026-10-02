#![cfg(test)]

use tuwunel_core::{
	Result,
	pdu::PduBuilder,
	ruma::{
		UserId,
		api::error::ErrorKind,
		events::room::member::{MembershipState, RoomMemberEventContent},
	},
};
use tuwunel_service::{Services, users::Register};

use self::{fixture::boot, timeline::append_pdu};

#[expect(dead_code)] // The fixture uses only the readiness probe.
mod client;

mod fixture;
#[path = "admin_grant_registration/probes.rs"]
mod probes;

#[expect(dead_code)] // Only membership events are appended.
mod timeline;

#[test]
fn refuses_names_once_joined_to_admin_room() -> Result {
	let options =
		["create_admin_room=true", "grant_admin_to_first_user=true", "log_enable=false"];

	boot("admin-grant-registration", options, exercise)
}

async fn exercise(services: &Services, _: &str) -> Result {
	let local = |name| UserId::parse_with_server_name(name, services.globals.server_name());
	let first = local("first")?;

	assert!(registered_admin(services, &first).await?);

	let ghost = local("ghost")?;
	let server_user = &services.globals.server_user;

	member(services, server_user, &ghost, MembershipState::Invite).await?;
	member(services, &ghost, &ghost, MembershipState::Join).await?;

	assert!(services.admin.user_is_admin(&ghost).await);
	assert!(once_joined(services, &ghost).await?);

	refused(services, &ghost).await?;

	assert!(services.admin.user_is_admin(&ghost).await);

	services.admin.revoke_admin(&ghost).await?;

	assert!(!services.admin.user_is_admin(&ghost).await);
	assert!(once_joined(services, &ghost).await?);

	refused(services, &ghost).await?;

	let invited = local("invited")?;

	member(services, server_user, &invited, MembershipState::Invite).await?;

	assert!(!once_joined(services, &invited).await?);

	assert!(!registered_admin(services, &invited).await?);

	let fresh = local("fresh")?;

	assert!(!registered_admin(services, &fresh).await?);

	Ok(())
}

async fn registered_admin(services: &Services, user_id: &UserId) -> Result<bool> {
	register(services, user_id).await?;

	assert!(services.users.exists(user_id).await);

	let admin = services.admin.user_is_admin(user_id).await;

	Ok(admin)
}

async fn register(services: &Services, user_id: &UserId) -> Result {
	services
		.users
		.full_register(Register {
			user_id: Some(user_id),
			password: Some("admin-grant-registration-password"),
			grant_first_user_admin: true,
			..Default::default()
		})
		.await
}

async fn member(
	services: &Services,
	sender: &UserId,
	user_id: &UserId,
	membership: MembershipState,
) -> Result {
	let admin_room = services.admin.get_admin_room().await?;
	let event = PduBuilder::state(user_id.as_str(), &RoomMemberEventContent::new(membership));

	append_pdu(services, sender, &admin_room, event)
		.await
		.map(drop)
}

async fn once_joined(services: &Services, user_id: &UserId) -> Result<bool> {
	let admin_room = services.admin.get_admin_room().await?;

	let joined = services
		.state_cache
		.once_joined(user_id, &admin_room)
		.await;

	Ok(joined)
}

async fn refused(services: &Services, user_id: &UserId) -> Result {
	let error = register(services, user_id)
		.await
		.expect_err("joined name registered");

	assert_eq!(error.kind(), ErrorKind::UserInUse);
	assert!(!services.users.exists(user_id).await);

	let error = services
		.users
		.create(user_id, None, None)
		.await
		.expect_err("joined name created");

	assert_eq!(error.kind(), ErrorKind::UserInUse);
	assert!(!services.users.exists(user_id).await);

	Ok(())
}
