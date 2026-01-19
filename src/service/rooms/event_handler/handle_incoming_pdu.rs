use futures::{FutureExt, TryFutureExt, TryStreamExt, future::try_join5};
use ruma::{CanonicalJsonObject, EventId, RoomId, ServerName, UserId, events::StateEventType};
use tuwunel_core::{
	Err, Result, debug,
	debug::INFO_SPAN_LEVEL,
	err, implement,
	matrix::{Event, room_version},
	trace,
	utils::{BoolExt, stream::IterStream},
	warn,
};

use crate::rooms::timeline::RawPduId;

/// When receiving an event one needs to:
/// 0. Check the server is in the room
/// 1. Skip the PDU if we already know about it
/// 1.1. Remove unsigned field
/// 2. Check signatures, otherwise drop
/// 3. Check content hash, redact if doesn't match
/// 4. Fetch any missing auth events doing all checks listed here starting at 1.
///    These are not timeline events
/// 5. Reject "due to auth events" if can't get all the auth events or some of
///    the auth events are also rejected "due to auth events"
/// 6. Reject "due to auth events" if the event doesn't pass auth based on the
///    auth events
/// 7. Persist this event as an outlier
/// 8. If not timeline event: stop
/// 9. Fetch any missing prev events doing all checks listed here starting at 1.
///    These are timeline events
/// 10. Fetch missing state and auth chain events by calling `/state_ids` at
///     backwards extremities doing all the checks in this list starting at
///     1. These are not timeline events
/// 11. Check the auth of the event passes based on the state of the event
/// 12. Ensure that the state is derived from the previous current state (i.e.
///     we calculated by doing state res where one of the inputs was a
///     previously trusted set of state, don't just trust a set of state we got
///     from a remote)
/// 13. Use state resolution to find new room state
/// 14. Check if the event passes auth based on the "current state" of the room,
///     if not soft fail it
#[implement(super::Service)]
#[tracing::instrument(
	name = "pdu",
	level = INFO_SPAN_LEVEL,
	skip_all,
	fields(%room_id, %event_id),
	ret(level = "debug"),
)]
pub async fn handle_incoming_pdu<'a>(
	&'a self,
	origin: &'a ServerName,
	room_id: &'a RoomId,
	event_id: &'a EventId,
	pdu: CanonicalJsonObject,
	is_timeline_event: bool,
) -> Result<Option<(RawPduId, bool)>> {
	// 1. Skip the PDU if we already have it as a timeline event
	if let Ok(pdu_id) = self.services.timeline.get_pdu_id(event_id).await {
		debug!(?pdu_id, "Exists.");
		return Ok(Some((pdu_id, false)));
	}

	// 1.1 Check the server is in the room
	let meta_exists = self.services.metadata.exists(room_id).map(Ok);

	// 1.2 Check if the room is disabled
	let is_disabled = self
		.services
		.metadata
		.is_disabled(room_id)
		.map(Ok);

	// 1.3.1 Check room ACL on origin field/server
	let origin_acl_check = self.acl_check(origin, room_id);

	// 1.3.2 Check room ACL on sender's server name
	let sender: &UserId = pdu
		.get("sender")
		.try_into()
		.map_err(|e| err!(Request(InvalidParam("PDU does not have a valid sender key: {e}"))))?;

	let sender_acl_check = sender
		.server_name()
		.ne(origin)
		.then_async(|| self.acl_check(sender.server_name(), room_id));

	// Fetch create event
	let create_event =
		self.services
			.state_accessor
			.room_state_get(room_id, &StateEventType::RoomCreate, "");

	let (meta_exists, is_disabled, (), (), ref create_event) = try_join5(
		meta_exists,
		is_disabled,
		origin_acl_check,
		sender_acl_check.map(|o| o.unwrap_or(Ok(()))),
		create_event,
	)
	.await?;

	if !meta_exists {
		return Err!(Request(NotFound("Room is unknown to this server")));
	}

	if is_disabled {
		return Err!(Request(Forbidden("Federation of this room is disabled by this server.")));
	}

	let room_version = room_version::from_create_event(create_event)?;

	let (incoming_pdu, val) = self
		.handle_outlier_pdu(origin, room_id, event_id, pdu, &room_version, false)
		.await?;

	// 8. if not timeline event: stop
	if !is_timeline_event {
		debug!(
			kind = ?incoming_pdu.event_type(),
			"Not a timeline event.",
		);
		return Ok(None);
	}

	// Skip old events
	let first_ts_in_room = self
		.services
		.timeline
		.first_pdu_in_room(room_id)
		.await?
		.origin_server_ts();

	if incoming_pdu.origin_server_ts() < first_ts_in_room {
		debug!(
			origin_server_ts = ?incoming_pdu.origin_server_ts(),
			?first_ts_in_room,
			"Skipping old event."
		);
		return Ok(None);
	}

	// 9. Fetch any missing prev events doing all checks listed here starting at 1.
	//    These are timeline events
	let (sorted_prev_events, mut eventid_info) = self
		.fetch_prev(origin, room_id, incoming_pdu.prev_events(), &room_version, first_ts_in_room)
		.await?;

	trace!(
		events = sorted_prev_events.len(),
		event_ids = ?sorted_prev_events,
		"Handling previous events"
	);
	sorted_prev_events
		.iter()
		.try_stream()
		.map_ok(AsRef::as_ref)
		.try_for_each(|prev_id| {
			self.handle_prev_pdu(
				origin,
				room_id,
				event_id,
				eventid_info.remove(prev_id),
				&room_version,
				first_ts_in_room,
				prev_id,
				create_event.event_id(),
			)
			.inspect_err(move |e| {
				warn!("Prev {prev_id} failed: {e}");
				self.back_off(prev_id);
			})
			.map(|_| self.services.server.check_running())
		})
		.boxed()
		.await?;

	// Done with prev events, now handling the incoming event
	self.upgrade_outlier_to_timeline_pdu(
		origin,
		room_id,
		incoming_pdu,
		val,
		&room_version,
		create_event.event_id(),
	)
	.boxed()
	.await
}
