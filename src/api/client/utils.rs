use ruma::{EventId, RoomId, UserId, api::client::filter::FilterDefinition};
use tuwunel_core::{Err, Event, Result, warn};
use tuwunel_service::Services;

use crate::Ruma;

pub(crate) fn normalize_profile_fields(mut filter: FilterDefinition) -> FilterDefinition {
	filter.profile_fields.ids.sort_unstable();
	filter.profile_fields.ids.dedup();
	filter
}

pub(crate) async fn invite_check(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
) -> Result {
	if services.config.block_non_admin_invites && !services.admin.user_is_admin(sender_user).await
	{
		warn!("{sender_user} is not an admin and attempted to send an invite to {room_id}");
		return Err!(Request(Forbidden("Invites are not allowed on this server.")));
	}

	Ok(())
}

pub(crate) async fn is_self_redaction(
	services: &Services,
	user_id: &UserId,
	event_id: &EventId,
) -> bool {
	services
		.timeline
		.get_pdu(event_id)
		.await
		.is_ok_and(|target| target.sender() == user_id)
}

/// Whether the caller may change display names under `enable_set_displayname`.
///
/// Appservices and server admins are exempt, matching Synapse's exemption for
/// admins. `is_admin` is awaited only when the option is off and the caller
/// is not an appservice.
pub(crate) async fn may_set_displayname<T>(
	services: &Services,
	body: &Ruma<T>,
	is_admin: impl AsyncFnOnce() -> bool,
) -> bool
where
	T: Sync,
{
	services.config.enable_set_displayname || body.appservice_info.is_some() || is_admin().await
}
