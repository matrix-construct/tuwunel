use futures::{StreamExt, TryFutureExt};
use ruma::{
	CanonicalJsonObject, EventId, RoomId, RoomVersionId, ServerName, events::TimelineEventType,
};
use tuwunel_core::{
	Err, Result, debug, debug_info, err, implement,
	matrix::{Event, PduEvent, event::TypeExt, room_version},
	pdu::format::from_incoming_federation,
	ref_at, state_res, trace,
	utils::{future::TryExtExt, stream::IterStream},
	warn,
};

use super::check_room_id;

#[implement(super::Service)]
pub(super) async fn handle_outlier_pdu(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	event_id: &EventId,
	mut pdu_json: CanonicalJsonObject,
	room_version: &RoomVersionId,
	auth_events_known: bool,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	debug!(?event_id, ?auth_events_known, "handle outlier");

	// 1. Remove unsigned field
	pdu_json.remove("unsigned");

	// TODO: For RoomVersion6 we must check that Raw<..> is canonical do we
	// anywhere?: https://matrix.org/docs/spec/rooms/v6#canonical-json
	// 2. Check signatures, otherwise drop
	// 3. check content hash, redact if doesn't match
	let mut pdu_json = match self
		.services
		.server_keys
		.verify_event(&pdu_json, Some(room_version))
		.await
	{
		| Ok(ruma::signatures::Verified::All) => pdu_json,
		| Ok(ruma::signatures::Verified::Signatures) => {
			// Redact
			debug_info!("Calculated hash does not match (redaction): {event_id}");
			let Some(rules) = room_version.rules() else {
				return Err!(Request(UnsupportedRoomVersion(
					"Cannot redact event for unknown room version {room_version:?}."
				)));
			};

			let Ok(obj) = ruma::canonical_json::redact(pdu_json, &rules.redaction, None) else {
				return Err!(Request(InvalidParam("Redaction failed")));
			};

			// Skip the PDU if it is redacted and we already have it as an outlier event
			if self.services.timeline.pdu_exists(event_id).await {
				return Err!(Request(InvalidParam(
					"Event was redacted and we already knew about it"
				)));
			}

			obj
		},
		| Err(e) => {
			return Err!(Request(InvalidParam(debug_error!(
				"Signature verification failed for {event_id}: {e}"
			))));
		},
	};

	let room_rules = room_version::rules(room_version)?;

	state_res::check_pdu_format(&pdu_json, &room_rules.event_format)?;

	// Now that we have checked the signature and hashes we can make mutations and
	// convert to our PduEvent type.
	let event = from_incoming_federation(room_id, event_id, &mut pdu_json, &room_rules)?;

	check_room_id(room_id, &event)?;

	if !auth_events_known {
		// 4. fetch any missing auth events doing all checks listed here starting at 1.
		//    These are not timeline events
		// 5. Reject "due to auth events" if can't get all the auth events or some of
		//    the auth events are also rejected "due to auth events"
		// NOTE: Step 5 is not applied anymore because it failed too often
		debug!("Fetching auth events");
		Box::pin(self.fetch_auth(origin, room_id, event.auth_events(), room_version)).await;
	}

	// 6. Reject "due to auth events" if the event doesn't pass auth based on the
	//    auth events
	debug!("Checking based on auth events");

	let is_hydra = !room_rules
		.event_format
		.allow_room_create_in_auth_events;

	let not_create = *event.kind() != TimelineEventType::RoomCreate;
	let hydra_create_id = (not_create && is_hydra)
		.then(|| event.room_id().as_event_id().ok())
		.flatten();

	let auth_events: Vec<_> = event
		.auth_events()
		.chain(hydra_create_id.as_deref().into_iter())
		.stream()
		.filter_map(|auth_event_id| {
			self.event_fetch(auth_event_id)
				.inspect_err(move |e| warn!("Missing auth_event {auth_event_id}: {e}"))
				.ok()
		})
		.map(|auth_event| {
			let event_type = auth_event.event_type();
			let state_key = auth_event
				.state_key()
				.expect("all auth events have state_key");

			(event_type.with_state_key(state_key), auth_event)
		})
		.collect()
		.await;

	state_res::auth_check(
		&room_rules,
		&event,
		&async |event_id| self.event_fetch(&event_id).await,
		&async |event_type, state_key| {
			let target = event_type.with_state_key(state_key);
			auth_events
				.iter()
				.find(|(type_state_key, _)| *type_state_key == target)
				.map(ref_at!(1))
				.cloned()
				.ok_or_else(|| err!(Request(NotFound("state not found"))))
		},
	)
	.inspect_ok(|()| trace!("Validation successful."))
	.await?;

	// 7. Persist the event as an outlier.
	self.services
		.timeline
		.add_pdu_outlier(event.event_id(), &pdu_json);

	trace!("Added pdu as outlier.");

	Ok((event, pdu_json))
}
