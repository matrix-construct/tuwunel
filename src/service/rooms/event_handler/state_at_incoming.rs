use std::{collections::HashMap, sync::atomic::AtomicBool};

use futures::{
	FutureExt, StreamExt, TryFutureExt, TryStreamExt,
	future::{join, try_join},
};
use ruma::{EventId, OwnedEventId, RoomId, RoomVersionId};
use tuwunel_core::{
	Error, Result, debug, debug_warn, err, implement,
	itertools::Itertools,
	matrix::Event,
	pdu::check_room_id,
	ref_at, trace,
	utils::{
		option::OptionExt,
		stream::{BroadbandExt, IterStream, ReadyExt, TryBroadbandExt, WidebandExt},
	},
};

use crate::rooms::{
	short::{ShortStateHash, ShortStateKey},
	state_res::{AuthSet, StateMap},
};

enum ForkError {
	State(Error),
	Chain(Error),
}

// TODO: if we know the prev_events of the incoming event we can avoid the
#[implement(super::Service)]
// request and build the state from a known point and resolve if > 1 prev_event
#[tracing::instrument(name = "state1", level = "debug", skip_all)]
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
		.inspect_err(|e| debug_warn!(?prev_event_id, %e, "Missing state at prev_event."))
		.await
	else {
		return Ok(None);
	};

	debug!(?prev_event_id, ?prev_event_sstatehash, "Resolving state at prev_event.");

	let prev_event = self
		.services
		.timeline
		.get_pdu(prev_event_id)
		.map_err(|e| err!(Database("Could not find prev_event, but we know the state: {e:?}")));

	let state = self
		.services
		.state_accessor
		.state_full_ids_strict(prev_event_sstatehash)
		.try_collect::<HashMap<_, _>>()
		.map(|state| {
			state
				.inspect_err(|error| {
					debug_warn!(
						%prev_event_id,
						?prev_event_sstatehash,
						%error,
						"Failed to materialize state at prev_event.",
					);
				})
				.ok()
		})
		.map(Ok);

	let (prev_event, state) = try_join(prev_event, state).await?;

	check_room_id(&prev_event, incoming_pdu.room_id())?;

	let Some(mut state) = state else {
		return Ok(None);
	};

	debug!(
		?prev_event_id,
		?prev_event_sstatehash,
		state_ids = state.len(),
		"Resolved state at prev_event.",
	);

	// Every event holding a `shorteventid_shortstatehash` row passed spec check 5
	// (auth against the state at its own position) as a hard reject; soft failure
	// (spec check 6) still writes the row, so soft-failed events are valid fold
	// inputs while positionally rejected events never gain a row.
	if let Some(state_key) = prev_event.state_key() {
		let prev_event_type = prev_event.event_type().to_cow_str().into();

		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&prev_event_type, state_key)
			.await;

		state.insert(shortstatekey, prev_event.event_id().into());
		// Now it's the state after the pdu
		debug!(
			?prev_event_id,
			?prev_event_type,
			?prev_event_sstatehash,
			?shortstatekey,
			state_ids = state.len(),
			"Added prev_event to state.",
		);
	}

	debug_assert!(!state.is_empty(), "should be returning None for empty HashMap result");

	Ok(Some(state))
}

#[implement(super::Service)]
#[tracing::instrument(name = "stateN", level = "debug", skip_all)]
pub(super) async fn state_at_incoming_resolved<Pdu>(
	&self,
	incoming_pdu: &Pdu,
	room_id: &RoomId,
	room_version: &RoomVersionId,
) -> Result<Option<HashMap<u64, OwnedEventId>>>
where
	Pdu: Event,
{
	debug_assert!(
		incoming_pdu.prev_events().count() > 1,
		"Incoming PDU should have more than one prev_event for this codepath"
	);

	trace!("Calculating extremity statehashes...");
	let Ok(prevs) = incoming_pdu
		.prev_events()
		.try_stream()
		.broad_and_then(|prev_event_id| {
			let sstatehash = self
				.services
				.state
				.pdu_shortstatehash(prev_event_id);

			let prev_event = self.services.timeline.get_pdu(prev_event_id);

			try_join(sstatehash, prev_event).inspect_err(move |e| {
				debug_warn!(?prev_event_id, %e, "Missing state at prev_event.");
			})
		})
		.try_collect::<Vec<_>>()
		.await
	else {
		return Ok(None);
	};

	trace!("Calculating fork states...");
	prevs
		.iter()
		.try_for_each(|(_, pdu)| check_room_id(pdu, room_id))?;

	let forks = prevs
		.into_iter()
		.sorted_unstable_by(|a, b| fork_identity(a).cmp(&fork_identity(b)))
		.dedup_by(|a, b| fork_identity(a) == fork_identity(b))
		.stream()
		.wide_then(|(sstatehash, prev_event)| {
			self.state_at_incoming_fork(room_id, room_version, sstatehash, prev_event)
		})
		.map(|fork| match fork {
			| Ok(fork) => Ok(Ok(fork)),
			| Err(ForkError::State(error)) => Ok(Err(error)),
			| Err(ForkError::Chain(error)) => Err(error),
		})
		.try_collect::<Vec<_>>()
		.await?;

	let Ok((fork_states, auth_chain_sets)): Result<(Vec<_>, Vec<_>)> = forks
		.into_iter()
		.process_results(|complete| complete.unzip())
		.inspect_err(|error| debug_warn!(%error, "Failed to materialize sibling fork state."))
	else {
		return Ok(None);
	};

	let fork_states = fork_states.into_iter().stream();
	let auth_chain_sets = auth_chain_sets.into_iter().stream();

	trace!("Resolving state");
	let Ok(new_state) = self
		.state_resolution(room_id, room_version, fork_states, auth_chain_sets, None)
		.inspect_ok(|_| trace!("State resolution done."))
		.await
	else {
		return Ok(None);
	};

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
		.collect::<HashMap<_, _>>()
		.inspect(|state| trace!(state = state.len(), "Created shortstatekeys."))
		.map(Some)
		.map(Ok)
		.await
}

/// The fork a prev event contributes to resolution.
///
/// A prev's snapshot is the state before it, so a state prev also carries its
/// own event; siblings sharing a snapshot share a fork only when neither sets
/// state.
fn fork_identity<Pdu>(
	(sstatehash, prev_event): &(ShortStateHash, Pdu),
) -> (ShortStateHash, Option<&EventId>)
where
	Pdu: Event,
{
	let leaf = prev_event
		.state_key()
		.map(|_| prev_event.event_id());

	(*sstatehash, leaf)
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
	room_version: &RoomVersionId,
	sstatehash: ShortStateHash,
	prev_event: Pdu,
) -> Result<(StateMap<OwnedEventId>, AuthSet<OwnedEventId>), ForkError>
where
	Pdu: Event,
{
	// Every event holding a `shorteventid_shortstatehash` row passed spec check 5
	// (auth against the state at its own position) as a hard reject; soft failure
	// (spec check 6) still writes the row, so soft-failed events are valid fold
	// inputs while positionally rejected events never gain a row.
	let leaf = prev_event
		.state_key()
		.map_stream(async |state_key| {
			let event_id = prev_event.event_id();
			let event_type = prev_event.kind().to_cow_str().into();
			let shortstatekey = self
				.services
				.short
				.get_or_create_shortstatekey(&event_type, state_key)
				.await;

			(shortstatekey, event_id.to_owned())
		});

	let snapshot = self
		.services
		.state_accessor
		.state_full_ids_complete(sstatehash)
		.map_err(ForkError::State)
		.await?;

	// The leaf goes last so `fork_state` lets it replace the snapshot's entry.
	let leaf_state_after_event = leaf
		.ready_fold(snapshot, |mut state, entry| {
			state.push(entry);
			state
		})
		.await;

	trace!(
		prev_event = ?prev_event.event_id(),
		?sstatehash,
		leaf_states = leaf_state_after_event.len(),
		"leaf state after event"
	);

	let state = leaf_state_after_event
		.iter()
		.map(|(shortstatekey, event_id)| (*shortstatekey, event_id));

	let starting_events = leaf_state_after_event
		.iter()
		.map(ref_at!(1))
		.map(AsRef::as_ref);

	let (state, chain) =
		join(self.fork_state(state), self.fork_chain(room_id, room_version, starting_events))
			.await;

	let chain = chain.map_err(ForkError::Chain)?;
	let state = state.map_err(ForkError::State)?;

	Ok((state, chain))
}

/// Converts one fork branch from short state keys to typed state keys.
///
/// A later duplicate state key replaces an earlier entry. Short state key
/// lookup failures are returned without constructing a partial map.
#[implement(super::Service)]
pub(super) async fn fork_state<'a, State>(
	&'a self,
	state: State,
) -> Result<StateMap<OwnedEventId>>
where
	State: Iterator<Item = (ShortStateKey, &'a OwnedEventId)> + Send + 'a,
{
	state
		.stream()
		.wide_then(|(k, id)| {
			self.services
				.short
				.get_statekey_from_short(k)
				.map_ok(|(ty, sk)| ((ty, sk), id.clone()))
		})
		.try_collect()
		.await
}

/// Collects the full auth chain for one fork branch.
///
/// The chain walk dedups short ids, so the ids are distinct unless a damaged
/// short-id mapping repeats one, which the auth difference counts once.
/// Iteration order is arbitrary.
#[implement(super::Service)]
pub(super) async fn fork_chain<'a, Events>(
	&'a self,
	room_id: &'a RoomId,
	room_version: &'a RoomVersionId,
	starting_events: Events,
) -> Result<AuthSet<OwnedEventId>>
where
	Events: Iterator<Item = &'a EventId> + Clone + ExactSizeIterator + Send + 'a,
{
	self.services
		.auth_chain
		.event_ids_iter(room_id, room_version, starting_events)
		.try_collect()
		.map_ok(AuthSet::from_distinct)
		.await
}

/// Collects the strict full auth chain for one fork branch.
///
/// The shared completeness flag is cleared when a polled chain source cannot
/// be loaded.
#[implement(super::Service)]
pub(super) async fn fork_chain_strict<'a, Events>(
	&'a self,
	room_id: &'a RoomId,
	room_version: &'a RoomVersionId,
	starting_events: Events,
	complete: &'a AtomicBool,
) -> Result<AuthSet<OwnedEventId>>
where
	Events: Iterator<Item = &'a EventId> + Clone + ExactSizeIterator + Send + 'a,
{
	self.services
		.auth_chain
		.event_ids_iter_strict(room_id, room_version, starting_events, complete)
		.try_collect()
		.map_ok(AuthSet::from_distinct)
		.await
}
