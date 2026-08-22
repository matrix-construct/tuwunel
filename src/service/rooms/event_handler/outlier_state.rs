use std::{collections::HashMap, sync::Arc};

use futures::StreamExt;
use ruma::{EventId, OwnedEventId, RoomId, events::StateEventType};
use tuwunel_core::{debug_warn, implement};
use tuwunel_database::Deserialized;

use crate::rooms::{short::ShortStateHash, state_compressor::CompressedState};

/// Caches state resolved for an event outside the timeline by event id.
///
/// The value is a non-authoritative compressor pointer that avoids a repeat
/// `/state_ids` fetch. A successful positional check can later promote the
/// materialized state into the event's authoritative state row.
#[implement(super::Service)]
pub(super) async fn cached_resolved_state(
	&self,
	event_id: &EventId,
) -> Option<HashMap<u64, OwnedEventId>> {
	let shortstatehash: ShortStateHash = self
		.db
		.eventid_resolvedstate
		.get(event_id)
		.await
		.deserialized()
		.ok()?;

	let state: HashMap<u64, OwnedEventId> = self
		.services
		.state_accessor
		.state_full_ids(shortstatehash)
		.collect()
		.await;

	// A room purge drops the events this map names; the create event goes only in
	// a full purge, so reject the hit when it is gone and let the caller refetch.
	let create_shortstatekey = self
		.services
		.short
		.get_shortstatekey(&StateEventType::RoomCreate, "")
		.await
		.ok()?;

	let create_event_id = state.get(&create_shortstatekey)?;
	let create_present = self
		.services
		.timeline
		.pdu_exists(create_event_id)
		.await;

	create_present.then_some(state)
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
