use std::borrow::Borrow;

use futures::{Stream, StreamExt, TryFutureExt};
use ruma::{EventId, RoomId, events::StateEventType};
use serde::Deserialize;
use tuwunel_core::{
	Result, err, implement,
	matrix::{Event, Pdu, StateKey},
};

/// Returns a single PDU from `room_id` with key (`event_type`,`state_key`).
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

/// Returns the full room state.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_state_full<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = Result<((StateEventType, StateKey), impl Event)>> + Send + 'a {
	self.services
		.state
		.get_room_shortstatehash(room_id)
		.map_ok(|shortstatehash| self.state_full(shortstatehash).map(Ok).boxed())
		.map_err(move |e| err!(Database("Missing state for {room_id:?}: {e:?}")))
		.try_flatten_stream()
}

/// Returns the full room state pdus
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_state_full_pdus<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = Result<impl Event>> + Send + 'a {
	self.services
		.state
		.get_room_shortstatehash(room_id)
		.map_ok(|shortstatehash| {
			self.state_full_pdus(shortstatehash)
				.map(Ok)
				.boxed()
		})
		.map_err(move |e| err!(Database("Missing state for {room_id:?}: {e:?}")))
		.try_flatten_stream()
}

/// Returns a single EventId from `room_id` with key (`event_type`,
/// `state_key`).
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn room_state_get_id<Id>(
	&self,
	room_id: &RoomId,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<Id>
where
	Id: for<'de> Deserialize<'de> + Send + Sized + ToOwned,
	<Id as ToOwned>::Owned: Borrow<EventId>,
{
	self.services
		.state
		.get_room_shortstatehash(room_id)
		.and_then(|shortstatehash| self.state_get_id(shortstatehash, event_type, state_key))
		.await
}

/// Returns a single PDU from `room_id` with key (`event_type`,
/// `state_key`).
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
