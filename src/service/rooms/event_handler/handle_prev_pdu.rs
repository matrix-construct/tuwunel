use futures::FutureExt;
use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, RoomId, RoomVersionId, ServerName,
};
use tuwunel_core::{Err, Result, debug, debug::INFO_SPAN_LEVEL, debug_warn, implement};
use tuwunel_matrix::{Event, PduEvent, pdu::RawPduId};

use super::backoff::{Context, UPGRADE_RETRY};

/// Context of an incoming event, shared across fetching and upgrading its
/// previous events, and upgrading the event itself.
///
/// `event_id` always names the incoming event; the event being upgraded, a
/// previous one or the incoming one itself, is a separate argument.
#[derive(Clone, Copy)]
pub(super) struct PrevUpgrade<'a> {
	pub(super) origin: &'a ServerName,
	pub(super) room_id: &'a RoomId,
	pub(super) event_id: &'a EventId,
	pub(super) room_version: &'a RoomVersionId,
	pub(super) recursion_level: usize,
	pub(super) first_ts_in_room: MilliSecondsSinceUnixEpoch,
	pub(super) create_event_id: &'a EventId,
}

#[implement(super::Service)]
#[tracing::instrument(
	name = "prev",
	level = INFO_SPAN_LEVEL,
	skip_all,
	fields(
		%prev_id,
	),
)]
pub(super) async fn handle_prev_pdu(
	&self,
	upgrade: PrevUpgrade<'_>,
	eventid_info: Option<(PduEvent, CanonicalJsonObject)>,
	prev_id: &EventId,
) -> Result<Option<(RawPduId, bool)>> {
	// Check for disabled again because it might have changed
	if self
		.services
		.metadata
		.is_disabled(upgrade.room_id)
		.await
	{
		let PrevUpgrade { origin, room_id, event_id, .. } = upgrade;

		return Err!(Request(Forbidden(debug_warn!(
			"Federation of room {room_id} is currently disabled on this server. Request by \
			 origin {origin} and event ID {event_id}"
		))));
	}

	let Some((pdu, json)) = eventid_info else {
		debug!(?prev_id, "Missing eventid_info.");
		return Ok(None);
	};

	// Skip old events
	if pdu.origin_server_ts() < upgrade.first_ts_in_room {
		debug_warn!(?prev_id, "origin_server_ts older than room");
		return Ok(None);
	}

	if self
		.is_suppressed(Context::Upgrade, prev_id, UPGRADE_RETRY)
		.await
		.is_deny()
	{
		debug!(?prev_id, "Backing off from prev_event");
		return Ok(None);
	}

	self.record_attempt(Context::Upgrade, prev_id);

	self.upgrade_outlier_to_timeline_pdu(upgrade, pdu, json)
		.boxed() // size firewall
		.await
}
