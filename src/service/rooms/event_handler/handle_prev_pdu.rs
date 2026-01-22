use std::{ops::Range, time::Duration};

use futures::FutureExt;
use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, RoomId, RoomVersionId, ServerName,
};
use tuwunel_core::{
	Err, Result, debug,
	debug::INFO_SPAN_LEVEL,
	implement,
	matrix::{Event, PduEvent},
};

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
	first_ts_in_room: MilliSecondsSinceUnixEpoch,
	prev_id: &EventId,
	create_event_id: &EventId,
) -> Result {
	// Check for disabled again because it might have changed
	if self.services.metadata.is_disabled(room_id).await {
		return Err!(Request(Forbidden(debug_warn!(
			"Federaton of room {room_id} is currently disabled on this server. Request by \
			 origin {origin} and event ID {event_id}"
		))));
	}

	if self.is_backed_off(prev_id, Range {
		start: Duration::from_secs(5 * 60),
		end: Duration::from_secs(60 * 60 * 24),
	}) {
		debug!(?prev_id, "Backing off from prev_event");
		return Ok(());
	}

	let Some((pdu, json)) = eventid_info else {
		return Ok(());
	};

	// Skip old events
	if pdu.origin_server_ts() < first_ts_in_room {
		return Ok(());
	}

	self.upgrade_outlier_to_timeline_pdu(
		origin,
		room_id,
		pdu,
		json,
		room_version,
		create_event_id,
	)
	.boxed()
	.await?;

	Ok(())
}
