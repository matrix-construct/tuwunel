use std::{borrow::Borrow, collections::HashMap, sync::Arc};

use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{OwnedEventId, RoomId, RoomVersionId};
use tuwunel_core::{
	Result, err, implement,
	matrix::room_version,
	state_res::{self, AuthSet, StateMap},
	trace,
	utils::stream::{IterStream, ReadyExt, TryWidebandExt, WidebandExt},
};

use crate::rooms::state_compressor::CompressedState;

#[implement(super::Service)]
#[tracing::instrument(
	name = "state",
	level = "debug",
	skip_all,
	fields(
		incoming = ?incoming_state.len()
	),
)]
pub async fn resolve_state(
	&self,
	room_id: &RoomId,
	room_version: &RoomVersionId,
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
				.event_ids_iter(room_id, room_version, state.values().map(Borrow::borrow))
				.try_collect::<AuthSet<OwnedEventId>>()
		})
		.ready_filter_map(Result::ok);

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
				.collect::<StateMap<OwnedEventId>>()
		});

	trace!("Resolving state");
	let state = self
		.state_resolution(room_id, room_version, fork_states, auth_chain_sets)
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
				.map(|(ssk, eid)| (ssk, (*eid).borrow())),
		)
		.collect()
		.await;

	Ok(Arc::new(new_room_state))
}

#[implement(super::Service)]
#[tracing::instrument(
	name = "resolve",
	level = "debug",
	skip_all,
	fields(%room_id),
)]
pub(super) async fn state_resolution<StateSets, AuthSets>(
	&self,
	room_id: &RoomId,
	room_version: &RoomVersionId,
	state_sets: StateSets,
	auth_chains: AuthSets,
) -> Result<StateMap<OwnedEventId>>
where
	StateSets: Stream<Item = StateMap<OwnedEventId>> + Send,
	AuthSets: Stream<Item = AuthSet<OwnedEventId>> + Send,
{
	state_res::resolve(
		&room_version::rules(room_version)?,
		state_sets,
		auth_chains,
		&async |event_id: OwnedEventId| self.event_fetch(&event_id).await,
		&async |event_id: OwnedEventId| self.event_exists(&event_id).await,
		self.services.server.config.hydra_backports,
	)
	.map_err(|e| err!(error!("State resolution failed: {e:?}")))
	.await
}
