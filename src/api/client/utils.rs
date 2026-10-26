use std::time::Duration;

use ruma::{EventId, RoomId, UserId, api::client::filter::FilterDefinition};
use tokio::time::Instant;
use tuwunel_core::{Err, Result, utils::time::millis, warn};
use tuwunel_matrix::Event;
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
	if services.config.rooms.invites.block_non_admin
		&& !services.admin.user_is_admin(sender_user).await
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

/// Whether the caller may change display names under
/// `profile.enable_set_displayname`.
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
	services.config.profile.enable_set_displayname
		|| body.appservice_info.is_some()
		|| is_admin().await
}

/// Whether the sender of a request is a guest.
///
/// A request on an appservice's own token is never a guest's, whichever user
/// it acts as.
pub(crate) async fn sender_is_guest<T>(services: &Services, body: &Ruma<T>) -> bool
where
	T: Sync,
{
	body.appservice_info.is_none() && services.users.is_guest(body.sender_user()).await
}

/// When the long poll of a sync-like request gives up.
///
/// A request without a timeout takes the configured default, and every
/// timeout is held between the configured minimum and maximum.
pub(crate) fn sync_deadline(services: &Services, timeout: Option<Duration>) -> Instant {
	let config = &services.config;
	let timeout = timeout
		.and_then(millis)
		.unwrap_or(config.client.sync.timeout_default)
		.max(config.client.sync.timeout_min)
		.min(config.client.sync.timeout_max);

	poll_deadline(timeout)
}

/// When a long poll of `timeout` milliseconds starting now gives up.
///
/// The configured maximum keeps every timeout small enough to add to now.
pub(crate) fn poll_deadline(timeout: u64) -> Instant {
	Instant::now()
		.checked_add(Duration::from_millis(timeout))
		.expect("configuration must limit maximum timeout")
}
