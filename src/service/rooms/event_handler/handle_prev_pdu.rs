use futures::FutureExt;
use ruma::{CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch};
use tuwunel_core::{Err, Result, debug, debug::INFO_SPAN_LEVEL, debug_warn, implement};
use tuwunel_matrix::{Event, PduEvent, pdu::RawPduId};

use super::{
	backoff::{Context, UPGRADE_RETRY},
	eval::Eval,
};

#[cfg(test)]
mod tests;

/// Upgrade `prev_id`, a previous event of the event `eval` evaluates.
///
/// A previous event older than the room's first event, or backing off after a
/// failed upgrade, is skipped. Otherwise a copy naming another ID than `prev_id`
/// fails as a storage error, since the upgrade resolves state for `prev_id`.
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
	eval @ Eval { origin, room_id, event_id, .. }: Eval<'_>,
	first_normal_ts: MilliSecondsSinceUnixEpoch,
	eventid_info: Option<(PduEvent, CanonicalJsonObject)>,
	prev_id: &EventId,
) -> Result<Option<(RawPduId, bool)>> {
	// Check for disabled again because it might have changed
	if self.services.metadata.is_disabled(room_id).await {
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
	if pdu.origin_server_ts() < first_normal_ts {
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

	if pdu.event_id() != prev_id {
		return Err!(Database("The stored previous event {prev_id} names another ID."));
	}

	self.record_attempt(Context::Upgrade, prev_id);

	let eval = Eval { event_id: prev_id, ..eval };

	self.upgrade_outlier_to_timeline_pdu(eval, pdu, json)
		.boxed() // size firewall
		.await
}
