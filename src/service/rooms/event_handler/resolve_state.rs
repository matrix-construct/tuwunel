use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	sync::Arc,
};

use conduwuit::{
	err, implement,
	state_res::{self, StateMap},
	trace,
	utils::stream::{automatic_width, IterStream, ReadyExt, TryWidebandExt, WidebandExt},
	Error, Result,
};
use futures::{future::try_join, FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{OwnedEventId, RoomId, RoomVersionId};

use crate::rooms::state_compressor::CompressedState;

#[implement(super::Service)]
#[tracing::instrument(name = "resolve", level = "debug", skip_all)]
pub async fn resolve_state(
	&self,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	incoming_state: HashMap<u64, OwnedEventId>,
) -> Result<Arc<CompressedState>> {
	trace!("Loading current room state ids");
	let current_sstatehash = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.map_err(|e| err!(Database(error!("No state for {room_id:?}: {e:?}"))))
		.await?;

	let current_state_ids: HashMap<_, _> = self
		.services
		.state_accessor
		.state_full_ids(current_sstatehash)
		.collect()
		.await;

	trace!("Loading fork states");
	let fork_states = [current_state_ids, incoming_state];
	let auth_chain_sets = fork_states
		.iter()
		.try_stream()
		.wide_and_then(|state| {
			self.services
				.auth_chain
				.event_ids_iter(room_id, state.values().map(Borrow::borrow))
				.try_collect()
		})
		.try_collect::<Vec<HashSet<OwnedEventId>>>();

	let fork_states = fork_states
		.iter()
		.stream()
		.wide_then(|fork_state| {
			let shortstatekeys = fork_state.keys().copied().stream();
			let event_ids = fork_state.values().cloned().stream();
			self.services
				.short
				.multi_get_statekey_from_short(shortstatekeys)
				.zip(event_ids)
				.ready_filter_map(|(ty_sk, id)| Some((ty_sk.ok()?, id)))
				.map(|((ty, sk), id)| ((ty, sk.as_str().to_owned()), id))
				.collect()
		})
		.map(Ok::<_, Error>)
		.try_collect::<Vec<StateMap<OwnedEventId>>>();

	let (fork_states, auth_chain_sets) = try_join(fork_states, auth_chain_sets).await?;

	trace!("Resolving state");
	let state = self
		.state_resolution(room_version_id, fork_states.iter(), &auth_chain_sets)
		.boxed()
		.await?;

	trace!("State resolution done.");
	let state_events: Vec<_> = state
		.iter()
		.stream()
		.wide_then(|((event_type, state_key), event_id)| {
			self.services
				.short
				.get_or_create_shortstatekey(event_type, state_key)
				.map(move |shortstatekey| (shortstatekey, event_id))
		})
		.collect()
		.await;

	trace!("Compressing state...");
	let new_room_state: CompressedState = self
		.services
		.state_compressor
		.compress_state_events(
			state_events
				.iter()
				.map(|(ref ssk, eid)| (ssk, (*eid).borrow())),
		)
		.collect()
		.await;

	Ok(Arc::new(new_room_state))
}

#[implement(super::Service)]
#[tracing::instrument(name = "ruma", level = "debug", skip_all)]
pub async fn state_resolution<'a, StateSets>(
	&'a self,
	room_version: &'a RoomVersionId,
	state_sets: StateSets,
	auth_chain_sets: &'a [HashSet<OwnedEventId>],
) -> Result<StateMap<OwnedEventId>>
where
	StateSets: Iterator<Item = &'a StateMap<OwnedEventId>> + Clone + Send,
{
	state_res::resolve(
		room_version,
		state_sets,
		auth_chain_sets,
		&|event_id| self.event_fetch(event_id),
		&|event_id| self.event_exists(event_id),
		automatic_width(),
	)
	.map_err(|e| err!(error!("State resolution failed: {e:?}")))
	.await
}
