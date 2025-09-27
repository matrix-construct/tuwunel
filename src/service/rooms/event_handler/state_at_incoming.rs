use std::{
	collections::HashMap,
	iter::{Iterator, once},
};

use futures::{
	FutureExt, StreamExt, TryFutureExt, TryStreamExt,
	future::{OptionFuture, try_join},
};
use ruma::{OwnedEventId, RoomId, RoomVersionId};
use tuwunel_core::{
	Result, apply, err, implement,
	matrix::{Event, StateMap, state_res::AuthSet},
	ref_at, trace,
	utils::stream::{BroadbandExt, IterStream, ReadyExt, TryBroadbandExt, TryWidebandExt},
};

use crate::rooms::short::ShortStateHash;

// TODO: if we know the prev_events of the incoming event we can avoid the
#[implement(super::Service)]
// request and build the state from a known point and resolve if > 1 prev_event
#[tracing::instrument(name = "state", level = "debug", skip_all)]
pub(super) async fn state_at_incoming_degree_one<Pdu>(
	&self,
	incoming_pdu: &Pdu,
) -> Result<Option<HashMap<u64, OwnedEventId>>>
where
	Pdu: Event,
{
	debug_assert!(
		incoming_pdu.prev_events().count() == 1,
		"Incoming PDU must have one prev_event to make this call"
	);

	let prev_event_id = incoming_pdu
		.prev_events()
		.next()
		.expect("at least one prev_event");

	let Ok(prev_event_sstatehash) = self
		.services
		.state
		.pdu_shortstatehash(prev_event_id)
		.await
	else {
		return Ok(None);
	};

	let prev_event = self
		.services
		.timeline
		.get_pdu(prev_event_id)
		.map_err(|e| err!(Database("Could not find prev_event, but we know the state: {e:?}")));

	let state = self
		.services
		.state_accessor
		.state_full_ids(prev_event_sstatehash)
		.collect::<HashMap<_, _>>()
		.map(Ok);

	let (prev_event, mut state) = try_join(prev_event, state).await?;

	if let Some(state_key) = prev_event.state_key() {
		let prev_event_type = prev_event.event_type().to_cow_str().into();

		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&prev_event_type, state_key)
			.await;

		state.insert(shortstatekey, prev_event.event_id().into());
		// Now it's the state after the pdu
	}

	debug_assert!(!state.is_empty(), "should be returning None for empty HashMap result");

	Ok(Some(state))
}

#[implement(super::Service)]
#[tracing::instrument(name = "state", level = "debug", skip_all)]
pub(super) async fn state_at_incoming_resolved<Pdu>(
	&self,
	incoming_pdu: &Pdu,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
) -> Result<Option<HashMap<u64, OwnedEventId>>>
where
	Pdu: Event,
{
	debug_assert!(
		incoming_pdu.prev_events().count() > 1,
		"Incoming PDU should have more than one prev_event for this codepath"
	);

	trace!("Calculating extremity statehashes...");
	let Ok(extremity_sstatehashes) = incoming_pdu
		.prev_events()
		.try_stream()
		.broad_and_then(|prev_event_id| {
			let prev_event = self.services.timeline.get_pdu(prev_event_id);

			let sstatehash = self
				.services
				.state
				.pdu_shortstatehash(prev_event_id);

			try_join(sstatehash, prev_event)
		})
		.try_collect::<HashMap<_, _>>()
		.await
	else {
		return Ok(None);
	};

	trace!("Calculating fork states...");
	let (fork_states, auth_chain_sets) = extremity_sstatehashes
		.into_iter()
		.try_stream()
		.wide_and_then(|(sstatehash, prev_event)| {
			self.state_at_incoming_fork(room_id, sstatehash, prev_event)
		})
		.try_collect()
		.map_ok(Vec::into_iter)
		.map_ok(Iterator::unzip)
		.map_ok(apply!(2, Vec::into_iter))
		.map_ok(apply!(2, IterStream::stream))
		.await?;

	trace!("Resolving state");
	let Ok(new_state) = self
		.state_resolution(room_version_id, fork_states, auth_chain_sets)
		.await
	else {
		return Ok(None);
	};

	trace!("State resolution done.");
	new_state
		.into_iter()
		.stream()
		.broad_then(async |((event_type, state_key), event_id)| {
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
#[tracing::instrument(
	name = "fork",
	level = "debug",
	skip_all,
	fields(
		?sstatehash,
		prev_event = ?prev_event.event_id(),
	)
)]
async fn state_at_incoming_fork<Pdu>(
	&self,
	room_id: &RoomId,
	sstatehash: ShortStateHash,
	prev_event: Pdu,
) -> Result<(StateMap<OwnedEventId>, AuthSet<OwnedEventId>)>
where
	Pdu: Event,
{
	let leaf: OptionFuture<_> = prev_event
		.state_key()
		.map(async |state_key| {
			self.services
				.short
				.get_or_create_shortstatekey(&prev_event.kind().to_cow_str().into(), state_key)
				.map(|shortstatekey| once((shortstatekey, prev_event.event_id().to_owned())))
				.await
		})
		.into();

	let leaf_state_after_event: Vec<_> = self
		.services
		.state_accessor
		.state_full_ids(sstatehash)
		.chain(leaf.await.into_iter().flatten().stream())
		.collect()
		.await;

	let starting_events = leaf_state_after_event
		.iter()
		.map(ref_at!(1))
		.map(AsRef::as_ref);

	let auth_chain = self
		.services
		.auth_chain
		.event_ids_iter(room_id, starting_events)
		.try_collect();

	let fork_state = leaf_state_after_event
		.iter()
		.stream()
		.broad_then(|(k, id)| {
			self.services
				.short
				.get_statekey_from_short(*k)
				.map_ok(|(ty, sk)| ((ty, sk), id.clone()))
		})
		.ready_filter_map(Result::ok)
		.collect()
		.map(Ok);

	try_join(fork_state, auth_chain).await
}
