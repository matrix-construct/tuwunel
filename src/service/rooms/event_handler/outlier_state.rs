use std::{collections::HashMap, sync::Arc};

use futures::TryStreamExt;
use ruma::{EventId, OwnedEventId, RoomId, events::StateEventType};
use tuwunel_core::{Result, debug_warn, implement};
use tuwunel_database::Deserialized;

use crate::rooms::{short::ShortStateHash, state_compressor::CompressedState};

type StateIds = HashMap<u64, OwnedEventId>;

/// Loads state resolved for an event outside the timeline by event id.
///
/// The value is a non-authoritative compressor pointer that avoids a repeat
/// `/state_ids` fetch. A successful positional check can later promote the
/// materialized state into the event's authoritative state row.
///
/// A hit requires strict state materialization and a live create-event canary.
/// Missing cache data remains a miss; materialization failures remain errors.
#[implement(super::Service)]
pub(super) async fn cached_resolved_state(&self, event_id: &EventId) -> Result<Option<StateIds>> {
	let Some(shortstatehash): Option<ShortStateHash> = optional_lookup(
		self.db
			.eventid_resolvedstate
			.get(event_id)
			.await
			.deserialized(),
	)?
	else {
		return Ok(None);
	};

	let state: StateIds = self
		.services
		.state_accessor
		.state_full_ids_strict(shortstatehash)
		.try_collect()
		.await?;

	// A room purge drops the events this map names; the create event goes only in
	// a full purge, so reject the hit when it is gone and let the caller refetch.
	let Some(create_shortstatekey) = optional_lookup(
		self.services
			.short
			.get_shortstatekey(&StateEventType::RoomCreate, "")
			.await,
	)?
	else {
		return Ok(None);
	};

	let Some(create_event_id) = state.get(&create_shortstatekey) else {
		return Ok(None);
	};

	let create_present = self
		.services
		.timeline
		.pdu_exists(create_event_id)
		.await;

	let state = create_present.then_some(state);

	Ok(state)
}

fn optional_lookup<T>(result: Result<T>) -> Result<Option<T>> {
	match result {
		| Ok(value) => Ok(Some(value)),
		| Err(error) if error.is_not_found() => Ok(None),
		| Err(error) => Err(error),
	}
}

/// Persist the state resolved for `event_id` over federation so a later walk of
/// the same event resolves without another fetch. Best effort: a failed
/// compressor write leaves the next walk to refetch.
#[implement(super::Service)]
pub(super) async fn cache_resolved_state(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	state: Arc<CompressedState>,
) {
	const BUFSIZE: usize = size_of::<ShortStateHash>();

	let Ok(saved) = self
		.services
		.state_compressor
		.save_state(room_id, state)
		.await
		.inspect_err(|e| debug_warn!(?event_id, "Failed to cache resolved state: {e}"))
	else {
		return;
	};

	self.db
		.eventid_resolvedstate
		.raw_aput::<BUFSIZE, _, _>(event_id, saved.shortstatehash);
}
