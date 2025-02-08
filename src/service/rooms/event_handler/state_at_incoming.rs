use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	iter::Iterator,
	sync::Arc,
};

use conduwuit::{
	debug, err, implement, trace,
	utils::stream::{BroadbandExt, IterStream, ReadyExt, TryBroadbandExt, TryWidebandExt},
	PduEvent, Result,
};
use futures::{future::try_join, FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{state_res::StateMap, OwnedEventId, RoomId, RoomVersionId};

use crate::rooms::short::ShortStateHash;

// TODO: if we know the prev_events of the incoming event we can avoid the
#[implement(super::Service)]
// request and build the state from a known point and resolve if > 1 prev_event
#[tracing::instrument(name = "state", level = "debug", skip_all)]
pub(super) async fn state_at_incoming_degree_one(
	&self,
	incoming_pdu: &Arc<PduEvent>,
) -> Result<Option<HashMap<u64, OwnedEventId>>> {
	let prev_event = &incoming_pdu.prev_events[0];
	let Ok(prev_event_sstatehash) = self
		.services
		.state_accessor
		.pdu_shortstatehash(prev_event)
		.await
	else {
		return Ok(None);
	};

	let mut state: HashMap<_, _> = self
		.services
		.state_accessor
		.state_full_ids(prev_event_sstatehash)
		.collect()
		.await;

	debug!("Using cached state");
	let prev_pdu = self
		.services
		.timeline
		.get_pdu(prev_event)
		.await
		.map_err(|e| err!(Database("Could not find prev event, but we know the state: {e:?}")))?;

	if let Some(state_key) = &prev_pdu.state_key {
		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&prev_pdu.kind.to_string().into(), state_key)
			.await;

		state.insert(shortstatekey, prev_event.clone());
		// Now it's the state after the pdu
	}

	debug_assert!(!state.is_empty(), "should be returning None for empty HashMap result");

	Ok(Some(state))
}

#[implement(super::Service)]
#[tracing::instrument(name = "state", level = "debug", skip_all)]
pub(super) async fn state_at_incoming_resolved(
	&self,
	incoming_pdu: &Arc<PduEvent>,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
) -> Result<Option<HashMap<u64, OwnedEventId>>> {
	trace!("Calculating extremity statehashes...");
	let Ok(extremity_sstatehashes) = incoming_pdu
		.prev_events
		.iter()
		.try_stream()
		.broad_and_then(|prev_eventid| {
			self.services
				.timeline
				.get_pdu(prev_eventid)
				.map_ok(move |prev_event| (prev_eventid, prev_event))
		})
		.broad_and_then(|(prev_eventid, prev_event)| {
			self.services
				.state_accessor
				.pdu_shortstatehash(prev_eventid)
				.map_ok(move |sstatehash| (sstatehash, prev_event))
		})
		.try_collect::<HashMap<_, _>>()
		.await
	else {
		return Ok(None);
	};

	trace!("Calculating fork states...");
	let (fork_states, auth_chain_sets): (Vec<StateMap<_>>, Vec<HashSet<_>>) =
		extremity_sstatehashes
			.into_iter()
			.try_stream()
			.wide_and_then(|(sstatehash, prev_event)| {
				self.state_at_incoming_fork(room_id, sstatehash, prev_event)
			})
			.try_collect()
			.map_ok(Vec::into_iter)
			.map_ok(Iterator::unzip)
			.await?;

	let Ok(new_state) = self
		.state_resolution(room_version_id, fork_states.iter(), &auth_chain_sets)
		.boxed()
		.await
	else {
		return Ok(None);
	};

	new_state
		.into_iter()
		.stream()
		.broad_then(|((event_type, state_key), event_id)| async move {
			self.services
				.short
				.get_or_create_shortstatekey(&event_type, &state_key)
				.map(move |shortstatekey| (shortstatekey, event_id))
				.await
		})
		.collect()
		.map(Some)
		.map(Ok)
		.await
}

#[implement(super::Service)]
async fn state_at_incoming_fork(
	&self,
	room_id: &RoomId,
	sstatehash: ShortStateHash,
	prev_event: PduEvent,
) -> Result<(StateMap<OwnedEventId>, HashSet<OwnedEventId>)> {
	let mut leaf_state: HashMap<_, _> = self
		.services
		.state_accessor
		.state_full_ids(sstatehash)
		.collect()
		.await;

	if let Some(state_key) = &prev_event.state_key {
		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&prev_event.kind.to_string().into(), state_key)
			.await;

		let event_id = &prev_event.event_id;
		leaf_state.insert(shortstatekey, event_id.clone());
		// Now it's the state after the pdu
	}

	let auth_chain = self
		.services
		.auth_chain
		.event_ids_iter(room_id, leaf_state.values().map(Borrow::borrow))
		.try_collect();

	let fork_state = leaf_state
		.iter()
		.stream()
		.broad_then(|(k, id)| {
			self.services
				.short
				.get_statekey_from_short(*k)
				.map_ok(|(ty, sk)| ((ty, sk), id.clone()))
				.map_ok(|((ty, sk), id)| ((ty, sk.as_str().to_owned()), id))
		})
		.ready_filter_map(Result::ok)
		.collect()
		.map(Ok);

	try_join(fork_state, auth_chain).await
}
