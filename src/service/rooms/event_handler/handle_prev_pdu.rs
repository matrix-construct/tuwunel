use futures::FutureExt;
use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, RoomId, RoomVersionId, ServerName,
};
use tuwunel_core::{
	Err, Result, debug,
	debug::INFO_SPAN_LEVEL,
	debug_warn, implement,
	matrix::{Event, PduEvent, pdu::RawPduId},
};

use super::backoff::{Context, UPGRADE_RETRY};

#[implement(super::Service)]
#[expect(clippy::too_many_arguments)]
#[tracing::instrument(
	name = "prev",
	level = INFO_SPAN_LEVEL,
	skip_all,
	fields(%prev_id),
)]
pub(super) async fn handle_prev_pdu(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	event_id: &EventId,
	eventid_info: Option<(PduEvent, CanonicalJsonObject)>,
	room_version: &RoomVersionId,
	recursion_level: usize,
	first_ts_in_room: MilliSecondsSinceUnixEpoch,
	prev_id: &EventId,
	create_event_id: &EventId,
) -> Result<Option<(RawPduId, bool)>> {
	// Check for disabled again because it might have changed
	if self.services.metadata.is_disabled(room_id).await {
		return Err!(Request(Forbidden(debug_warn!(
			"Federaton of room {room_id} is currently disabled on this server. Request by \
			 origin {origin} and event ID {event_id}"
		))));
	}

	let Some((pdu, json)) = eventid_info else {
		debug!(?prev_id, "Missing eventid_info.");
		return Ok(None);
	};

	// Skip old events
	if pdu.origin_server_ts() < first_ts_in_room {
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

	self.upgrade_outlier_to_timeline_pdu(
		origin,
		room_id,
		pdu,
		json,
		room_version,
		recursion_level,
		create_event_id,
	)
	.boxed() // size firewall
	.await
}
