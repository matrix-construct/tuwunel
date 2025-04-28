use std::{borrow::Borrow, collections::BTreeMap, iter::once, sync::Arc, time::Instant};

use futures::{FutureExt, StreamExt, future::ready};
use ruma::{CanonicalJsonValue, RoomId, ServerName, events::StateEventType};
use tuwunel_core::{
	Err, Result, debug, debug_info, err, implement,
	matrix::{EventTypeExt, PduEvent, StateKey, state_res},
	trace,
	utils::stream::{BroadbandExt, ReadyExt},
	warn,
};

use super::{get_room_version_id, to_room_version};
use crate::rooms::{
	state_compressor::{CompressedState, HashSetCompressStateEvent},
	timeline::RawPduId,
};

#[implement(super::Service)]
pub(super) async fn upgrade_outlier_to_timeline_pdu(
	&self,
	incoming_pdu: PduEvent,
	val: BTreeMap<String, CanonicalJsonValue>,
	create_event: &PduEvent,
	origin: &ServerName,
	room_id: &RoomId,
) -> Result<Option<RawPduId>> {
	// Skip the PDU if we already have it as a timeline event
	if let Ok(pduid) = self
		.services
		.timeline
		.get_pdu_id(&incoming_pdu.event_id)
		.await
	{
		return Ok(Some(pduid));
	}

	if self
		.services
		.pdu_metadata
		.is_event_soft_failed(&incoming_pdu.event_id)
		.await
	{
		return Err!(Request(InvalidParam("Event has been soft failed")));
	}

	debug!("Upgrading to timeline pdu");
	let timer = Instant::now();
	let room_version_id = get_room_version_id(create_event)?;

	// 10. Fetch missing state and auth chain events by calling /state_ids at
	//     backwards extremities doing all the checks in this list starting at 1.
	//     These are not timeline events.

	debug!("Resolving state at event");
	let mut state_at_incoming_event = if incoming_pdu.prev_events.len() == 1 {
		self.state_at_incoming_degree_one(&incoming_pdu)
			.await?
	} else {
		self.state_at_incoming_resolved(&incoming_pdu, room_id, &room_version_id)
			.await?
	};

	if state_at_incoming_event.is_none() {
		state_at_incoming_event = self
			.fetch_state(origin, create_event, room_id, &incoming_pdu.event_id)
			.await?;
	}

	let state_at_incoming_event =
		state_at_incoming_event.expect("we always set this to some above");
	let room_version = to_room_version(&room_version_id);

	debug!("Performing auth check");
	// 11. Check the auth of the event passes based on the state of the event
	let state_fetch_state = &state_at_incoming_event;
	let state_fetch = |k: StateEventType, s: StateKey| async move {
		let shortstatekey = self
			.services
			.short
			.get_shortstatekey(&k, &s)
			.await
			.ok()?;

		let event_id = state_fetch_state.get(&shortstatekey)?;
		self.services
			.timeline
			.get_pdu(event_id)
			.await
			.ok()
	};

	let auth_check = state_res::event_auth::auth_check(
		&room_version,
		&incoming_pdu,
		None, // TODO: third party invite
		|ty, sk| state_fetch(ty.clone(), sk.into()),
	)
	.await
	.map_err(|e| err!(Request(Forbidden("Auth check failed: {e:?}"))))?;

	if !auth_check {
		return Err!(Request(Forbidden("Event has failed auth check with state at the event.")));
	}

	debug!("Gathering auth events");
	let auth_events = self
		.services
		.state
		.get_auth_events(
			room_id,
			&incoming_pdu.kind,
			&incoming_pdu.sender,
			incoming_pdu.state_key.as_deref(),
			&incoming_pdu.content,
		)
		.await?;

	let state_fetch = |k: &StateEventType, s: &str| {
		let key = k.with_state_key(s);
		ready(auth_events.get(&key).cloned())
	};

	let auth_check = state_res::event_auth::auth_check(
		&room_version,
		&incoming_pdu,
		None, // third-party invite
		state_fetch,
	)
	.await
	.map_err(|e| err!(Request(Forbidden("Auth check failed: {e:?}"))))?;

	// Soft fail check before doing state res
	debug!("Performing soft-fail check");
	let soft_fail = match (auth_check, incoming_pdu.redacts_id(&room_version_id)) {
		| (false, _) => true,
		| (true, None) => false,
		| (true, Some(redact_id)) => {
			!self
				.services
				.state_accessor
				.user_can_redact(&redact_id, &incoming_pdu.sender, &incoming_pdu.room_id, true)
				.await?
		},
	};

	// 13. Use state resolution to find new room state

	// We start looking at current room state now, so lets lock the room
	trace!("Locking the room");
	let state_lock = self.services.state.mutex.lock(room_id).await;

	// Now we calculate the set of extremities this room has after the incoming
	// event has been applied. We start with the previous extremities (aka leaves)
	trace!("Calculating extremities");
	let extremities: Vec<_> = self
		.services
		.state
		.get_forward_extremities(room_id)
		.map(ToOwned::to_owned)
		.ready_filter(|event_id| {
			// Remove any that are referenced by this incoming event's prev_events
			!incoming_pdu.prev_events.contains(event_id)
		})
		.broad_filter_map(|event_id| async move {
			// Only keep those extremities were not referenced yet
			self.services
				.pdu_metadata
				.is_event_referenced(room_id, &event_id)
				.await
				.eq(&false)
				.then_some(event_id)
		})
		.collect()
		.await;

	debug!(
		"Retained {} extremities checked against {} prev_events",
		extremities.len(),
		incoming_pdu.prev_events.len()
	);

	let state_ids_compressed: Arc<CompressedState> = self
		.services
		.state_compressor
		.compress_state_events(
			state_at_incoming_event
				.iter()
				.map(|(ssk, eid)| (ssk, eid.borrow())),
		)
		.collect()
		.map(Arc::new)
		.await;

	if incoming_pdu.state_key.is_some() {
		debug!("Event is a state-event. Deriving new room state");

		// We also add state after incoming event to the fork states
		let mut state_after = state_at_incoming_event.clone();
		if let Some(state_key) = &incoming_pdu.state_key {
			let shortstatekey = self
				.services
				.short
				.get_or_create_shortstatekey(&incoming_pdu.kind.to_string().into(), state_key)
				.await;

			let event_id = &incoming_pdu.event_id;
			state_after.insert(shortstatekey, event_id.clone());
		}

		let new_room_state = self
			.resolve_state(room_id, &room_version_id, state_after)
			.await?;

		// Set the new room state to the resolved state
		debug!("Forcing new room state");
		let HashSetCompressStateEvent { shortstatehash, added, removed } = self
			.services
			.state_compressor
			.save_state(room_id, new_room_state)
			.await?;

		self.services
			.state
			.force_state(room_id, shortstatehash, added, removed, &state_lock)
			.await?;
	}

	// 14. Check if the event passes auth based on the "current state" of the room,
	//     if not soft fail it
	if soft_fail {
		debug!("Soft failing event");
		let extremities = extremities.iter().map(Borrow::borrow);

		self.services
			.timeline
			.append_incoming_pdu(
				&incoming_pdu,
				val,
				extremities,
				state_ids_compressed,
				soft_fail,
				&state_lock,
			)
			.await?;

		// Soft fail, we keep the event as an outlier but don't add it to the timeline
		self.services
			.pdu_metadata
			.mark_event_soft_failed(&incoming_pdu.event_id);

		warn!("Event was soft failed: {incoming_pdu:?}");
		return Err!(Request(InvalidParam("Event has been soft failed")));
	}

	// Now that the event has passed all auth it is added into the timeline.
	// We use the `state_at_event` instead of `state_after` so we accurately
	// represent the state for this event.
	trace!("Appending pdu to timeline");
	let extremities = extremities
		.iter()
		.map(Borrow::borrow)
		.chain(once(incoming_pdu.event_id.borrow()));

	let pdu_id = self
		.services
		.timeline
		.append_incoming_pdu(
			&incoming_pdu,
			val,
			extremities,
			state_ids_compressed,
			soft_fail,
			&state_lock,
		)
		.await?;

	// Event has passed all auth/stateres checks
	drop(state_lock);
	debug_info!(
		elapsed = ?timer.elapsed(),
		"Accepted",
	);

	Ok(pdu_id)
}
