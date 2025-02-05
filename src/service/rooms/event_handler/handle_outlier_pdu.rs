use std::{
	collections::{hash_map, BTreeMap, HashMap},
	sync::Arc,
};

use conduwuit::{
	debug, debug_info, err, implement, state_res, trace, warn, Err, Error, PduEvent, Result,
};
use futures::{future::ready, TryFutureExt};
use ruma::{
	api::client::error::ErrorKind, events::StateEventType, CanonicalJsonObject,
	CanonicalJsonValue, EventId, RoomId, ServerName,
};

use super::{check_room_id, get_room_version_id, to_room_version};

#[implement(super::Service)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_outlier_pdu<'a>(
	&self,
	origin: &'a ServerName,
	create_event: &'a PduEvent,
	event_id: &'a EventId,
	room_id: &'a RoomId,
	mut value: CanonicalJsonObject,
	auth_events_known: bool,
) -> Result<(Arc<PduEvent>, BTreeMap<String, CanonicalJsonValue>)> {
	// 1. Remove unsigned field
	value.remove("unsigned");

	// TODO: For RoomVersion6 we must check that Raw<..> is canonical do we anywhere?: https://matrix.org/docs/spec/rooms/v6#canonical-json

	// 2. Check signatures, otherwise drop
	// 3. check content hash, redact if doesn't match
	let room_version_id = get_room_version_id(create_event)?;
	let mut val = match self
		.services
		.server_keys
		.verify_event(&value, Some(&room_version_id))
		.await
	{
		| Ok(ruma::signatures::Verified::All) => value,
		| Ok(ruma::signatures::Verified::Signatures) => {
			// Redact
			debug_info!("Calculated hash does not match (redaction): {event_id}");
			let Ok(obj) = ruma::canonical_json::redact(value, &room_version_id, None) else {
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
		| Err(e) =>
			return Err!(Request(InvalidParam(debug_error!(
				"Signature verification failed for {event_id}: {e}"
			)))),
	};

	// Now that we have checked the signature and hashes we can add the eventID and
	// convert to our PduEvent type
	val.insert("event_id".to_owned(), CanonicalJsonValue::String(event_id.as_str().to_owned()));
	let incoming_pdu = serde_json::from_value::<PduEvent>(
		serde_json::to_value(&val).expect("CanonicalJsonObj is a valid JsonValue"),
	)
	.map_err(|e| err!(Request(BadJson(debug_warn!("Event is not a valid PDU: {e}")))))?;

	check_room_id(room_id, &incoming_pdu)?;

	if !auth_events_known {
		// 4. fetch any missing auth events doing all checks listed here starting at 1.
		//    These are not timeline events
		// 5. Reject "due to auth events" if can't get all the auth events or some of
		//    the auth events are also rejected "due to auth events"
		// NOTE: Step 5 is not applied anymore because it failed too often
		debug!("Fetching auth events");
		Box::pin(self.fetch_and_handle_outliers(
			origin,
			&incoming_pdu.auth_events,
			create_event,
			room_id,
		))
		.await;
	}

	// 6. Reject "due to auth events" if the event doesn't pass auth based on the
	//    auth events
	debug!("Checking based on auth events");
	// Build map of auth events
	let mut auth_events = HashMap::with_capacity(incoming_pdu.auth_events.len());
	for id in &incoming_pdu.auth_events {
		let Ok(auth_event) = self.services.timeline.get_pdu(id).map_ok(Arc::new).await else {
			warn!("Could not find auth event {id}");
			continue;
		};

		check_room_id(room_id, &auth_event)?;

		match auth_events.entry((
			auth_event.kind.to_string().into(),
			auth_event
				.state_key
				.clone()
				.expect("all auth events have state keys"),
		)) {
			| hash_map::Entry::Vacant(v) => {
				v.insert(auth_event);
			},
			| hash_map::Entry::Occupied(_) => {
				return Err(Error::BadRequest(
					ErrorKind::InvalidParam,
					"Auth event's type and state_key combination exists multiple times.",
				));
			},
		}
	}

	// The original create event must be in the auth events
	if !matches!(
		auth_events
			.get(&(StateEventType::RoomCreate, String::new().into()))
			.map(AsRef::as_ref),
		Some(_) | None
	) {
		return Err(Error::BadRequest(
			ErrorKind::InvalidParam,
			"Incoming event refers to wrong create event.",
		));
	}

	let state_fetch = |ty: &'static StateEventType, sk: &str| {
		let key = (ty.to_owned(), sk.into());
		ready(auth_events.get(&key))
	};

	let auth_check = state_res::event_auth::auth_check(
		&to_room_version(&room_version_id),
		&incoming_pdu,
		None, // TODO: third party invite
		state_fetch,
	)
	.await
	.map_err(|e| err!(Request(Forbidden("Auth check failed: {e:?}"))))?;

	if !auth_check {
		return Err!(Request(Forbidden("Auth check failed")));
	}

	trace!("Validation successful.");

	// 7. Persist the event as an outlier.
	self.services
		.outlier
		.add_pdu_outlier(&incoming_pdu.event_id, &val);

	trace!("Added pdu as outlier.");

	Ok((Arc::new(incoming_pdu), val))
}
