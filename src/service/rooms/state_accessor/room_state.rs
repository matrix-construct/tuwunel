//! Resolves the current state snapshot for a room.
//!
//! These adapters obtain a room's current short-state hash and delegate to the
//! historical snapshot readers. Stream variants surface a missing room snapshot
//! while retaining the delegated readers' best-effort item behavior.

use futures::{Stream, StreamExt, TryFutureExt};
use ruma::{OwnedEventId, RoomId, events::StateEventType};
use serde::Deserialize;
use tuwunel_core::{
	Result, err, implement,
	matrix::{Event, Pdu, StateKey},
};

/// Deserializes one current state event's content.
///
/// The event is selected by `(event_type, state_key)`. Snapshot lookup,
/// timeline lookup, and content errors are returned to the caller.
#[implement(super::Service)]
pub async fn room_state_get_content<T>(
	&self,
	room_id: &RoomId,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<T>
where
	T: for<'de> Deserialize<'de> + Send,
{
	self.room_state_get(room_id, event_type, state_key)
		.await
		.and_then(|event| event.get_content())
}

/// Streams current state events of one type.
///
/// Failure to resolve the room's current snapshot is yielded as an error.
/// Missing reverse mappings and unavailable PDUs are skipped by the delegated
/// best-effort stream.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_state_type_pdus<'a>(
	&'a self,
	room_id: &'a RoomId,
	event_type: &'a StateEventType,
) -> impl Stream<Item = Result<impl Event>> + Send + 'a {
	self.services
		.state
		.get_room_shortstatehash(room_id)
		.map_ok(|shortstatehash| {
			self.state_type_pdus(shortstatehash, event_type)
				.map(Ok)
		})
		.map_err(move |e| err!(Database("Missing state for {room_id:?}: {e:?}")))
		.try_flatten_stream()
}

/// Streams the room's full current state with type and state keys.
///
/// Failure to resolve the current snapshot is yielded as an error. Entries
/// whose IDs, PDUs, or state keys cannot be resolved are skipped.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_state_full<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = Result<((StateEventType, StateKey), impl Event)>> + Send + 'a {
	self.services
		.state
		.get_room_shortstatehash(room_id)
		.map_ok(|shortstatehash| self.state_full(shortstatehash).map(Ok))
		.map_err(move |e| err!(Database("Missing state for {room_id:?}: {e:?}")))
		.try_flatten_stream()
}

/// Streams every resolvable PDU in the room's current state.
///
/// Failure to resolve the current snapshot is yielded as an error. Individual
/// state entries with missing reverse mappings or PDUs are skipped.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_state_full_pdus<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = Result<impl Event>> + Send + 'a {
	self.services
		.state
		.get_room_shortstatehash(room_id)
		.map_ok(|shortstatehash| self.state_full_pdus(shortstatehash).map(Ok))
		.map_err(move |e| err!(Database("Missing state for {room_id:?}: {e:?}")))
		.try_flatten_stream()
}

/// Returns the event ID for one current state tuple.
///
/// The room's current snapshot must exist, and both short-ID mappings must
/// resolve for `(event_type, state_key)`.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn room_state_get_id(
	&self,
	room_id: &RoomId,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<OwnedEventId> {
	self.services
		.state
		.get_room_shortstatehash(room_id)
		.and_then(|shortstatehash| self.state_get_id(shortstatehash, event_type, state_key))
		.await
}

/// Streams state keys and event IDs for one current state event type.
///
/// Failure to resolve the current snapshot is yielded as an error. Individual
/// short-ID mapping failures are omitted by the best-effort inner stream.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_state_keys_with_ids<'a>(
	&'a self,
	room_id: &'a RoomId,
	event_type: &'a StateEventType,
) -> impl Stream<Item = Result<(StateKey, OwnedEventId)>> + Send + 'a {
	self.services
		.state
		.get_room_shortstatehash(room_id)
		.map_ok(|shortstatehash| {
			self.state_keys_with_ids(shortstatehash, event_type)
				.map(Ok)
		})
		.map_err(move |e| err!(Database("Missing state for {room_id:?}: {e:?}")))
		.try_flatten_stream()
}

/// Streams state keys for one current state event type.
///
/// Failure to resolve the current snapshot is yielded as an error. Individual
/// state-key mapping failures are omitted by the best-effort inner stream.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_state_keys<'a>(
	&'a self,
	room_id: &'a RoomId,
	event_type: &'a StateEventType,
) -> impl Stream<Item = Result<StateKey>> + Send + 'a {
	self.services
		.state
		.get_room_shortstatehash(room_id)
		.map_ok(|shortstatehash| {
			self.state_keys(shortstatehash, event_type)
				.map(Ok)
		})
		.map_err(move |e| err!(Database("Missing state for {room_id:?}: {e:?}")))
		.try_flatten_stream()
}

/// Returns one current state PDU.
///
/// The event is selected by `(event_type, state_key)`. Snapshot, short-ID, and
/// timeline lookup failures are returned to the caller.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn room_state_get(
	&self,
	room_id: &RoomId,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<Pdu> {
	self.services
		.state
		.get_room_shortstatehash(room_id)
		.and_then(|shortstatehash| self.state_get(shortstatehash, event_type, state_key))
		.await
}
