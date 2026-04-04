use ruma::{RoomId, UserId, api::client::error::ErrorKind::InviteBlocked};
use serde::Deserialize;
use tuwunel_core::{Err, Result, warn};
use tuwunel_service::Services;

#[derive(Debug, Deserialize)]
pub(crate) struct InvitePermissionConfig {
	#[serde(default)]
	pub(crate) default_action: InviteDefaultAction,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InviteDefaultAction {
	#[default]
	Allow,
	Block,
}
pub(crate) async fn invite_check(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
) -> Result {
	// when server admin block non admin invites, check if user is admin
	if !services.admin.user_is_admin(sender_user).await && services.config.block_non_admin_invites
	{
		warn!("{sender_user} is not an admin and attempted to send an invite to {room_id}");
		return Err!(Request(Forbidden("Invites are not allowed on this server.")));
	}

	Ok(())
}

pub(crate) async fn is_invite_blocked(services: &Services, target_user: &UserId) -> bool {
	// Check stable identifier
	if let Ok(config) = services
		.account_data
		.get_global::<InvitePermissionConfig>(target_user, "m.invite_permission_config".into())
		.await
	{
		if config.default_action == InviteDefaultAction::Block {
			return true;
		}
	}
	// TODO: when MSC4380 is stable, remove this
	// Check unstable identifier
	if let Ok(config) = services
		.account_data
		.get_global::<InvitePermissionConfig>(
			target_user,
			"org.matrix.msc4380.invite_permission_config".into(),
		)
		.await
	{
		if config.default_action == InviteDefaultAction::Block {
			return true;
		}
	}

	false
}
