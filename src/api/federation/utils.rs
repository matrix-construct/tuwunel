use std::{borrow::Borrow, iter::once};

use futures::{
	FutureExt, Stream, StreamExt,
	future::{join, join3, try_join},
};
use itertools::Itertools;
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId, ServerName};
use serde::Deserialize;
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{
	Err, Result, at, err, implement, is_false,
	result::NotFound,
	utils::{FutureBoolExt, IterStream, ReadyExt, future::ReadyBoolExt, option::OptionExt},
	warn,
};
use tuwunel_service::{Services, rooms::short::ShortEventId};

type StateAtEvent = (RoomVersionId, Vec<OwnedEventId>);

pub(super) struct AccessCheck<'a> {
	pub(super) services: &'a Services,
	pub(super) origin: &'a ServerName,
	pub(super) room_id: &'a RoomId,
	pub(super) event_id: Option<&'a EventId>,
}

#[implement(AccessCheck, params = "<'_>")]
pub(super) async fn check(&self) -> Result {
	let acl_check = self
		.services
		.event_handler
		.acl_check(self.origin, self.room_id)
		.map(|result| result.is_ok());

	let server_in_room = self
		.services
		.state_cache
		.server_in_room(self.origin, self.room_id);

	let world_readable = self
		.services
		.state_accessor
		.is_world_readable(self.room_id);

	let server_can_see = self.event_id.map_async(|event_id| {
		self.services
			.state_accessor
			.server_can_see_event(self.origin, self.room_id, event_id)
	});

	// The cheap membership probe leads; a hit there elides the other read.
	let room_unreachable = server_in_room
		.is_false()
		.and(world_readable.is_false());

	let (acl_check, room_unreachable, server_can_see) =
		join3(acl_check, room_unreachable, server_can_see).await;

	if !acl_check {
		return Err!(Request(Forbidden("Server access denied.")));
	}

	if room_unreachable {
		return Err!(Request(Forbidden("Server is not in room.")));
	}

	if server_can_see.is_some_and(is_false!()) {
		return Err!(Request(Forbidden("Server is not allowed to see event.")));
	}

	Ok(())
}

pub(super) async fn require_known_room(
	services: &Services,
	room_id: &RoomId,
	origin: &ServerName,
) -> Result {
	if !services.metadata.exists(room_id).await {
		return Err!(Request(NotFound("Room is unknown to this server.")));
	}

	services
		.event_handler
		.acl_check(origin, room_id)
		.await
}

/// Reads the state of a room at an event for a federation response.
///
/// The origin must pass the room's access check and the event must belong to
/// the room. The state comes back as event IDs, beside the room's version.
pub(super) async fn state_at_event(
	services: &Services,
	origin: &ServerName,
	room_id: &RoomId,
	event_id: &EventId,
) -> Result<StateAtEvent> {
	AccessCheck {
		services,
		origin,
		room_id,
		event_id: None,
	}
	.check()
	.await?;

	require_event_in_room(services, event_id, room_id).await?;

	let shortstatehash = services
		.state
		.pdu_shortstatehash(event_id)
		.await
		.map_err(|_| err!(Request(NotFound("PDU state not found."))))?;

	let state_ids = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.map(at!(1))
		.collect()
		.map(Ok);

	let room_version = services.state.get_room_version(room_id);

	try_join(room_version, state_ids).await
}

pub(super) async fn require_event_in_room(
	services: &Services,
	event_id: &EventId,
	room_id: &RoomId,
) -> Result {
	#[derive(Deserialize)]
	struct PduRoomId {
		room_id: OwnedRoomId,
	}

	services
		.timeline
		.get::<PduRoomId>(event_id)
		.await
		.is_ok_and(|pdu| pdu.room_id == room_id)
		.then_some(())
		.ok_or_else(|| err!(Request(NotFound("Event not found."))))
}

/// Streams the auth chain of the state at an event and of the event itself.
///
/// Once the room has forked, the event's own auth events can fall outside the
/// state's chain, and a peer validating the event needs them too. The two are
/// walked separately and merged, so requests at the same state can share its
/// cached walk.
pub(super) fn walk_state_auth_chain<'a>(
	services: &'a Services,
	room_id: &'a RoomId,
	room_version: &'a RoomVersionId,
	state: &'a [OwnedEventId],
	event_id: &'a EventId,
) -> impl Stream<Item = OwnedEventId> + Send + 'a {
	let state_heads = state.iter().map(Borrow::borrow);
	let state_chain = short_auth_chain(services, room_id, room_version, state_heads);
	let event_chain = short_auth_chain(services, room_id, room_version, once(event_id));

	let chain = join(state_chain, event_chain)
		.map(|(state_chain, event_chain)| state_chain.into_iter().merge(event_chain).dedup());

	auth_chain_ids(services, chain)
}

/// Streams the auth chain of the state a federation response serves.
///
/// A walk that fails is logged and yields no events, so the response goes out
/// without the chain rather than failing whole.
pub(super) fn walk_auth_chain<'a, I>(
	services: &'a Services,
	room_id: &'a RoomId,
	room_version: &'a RoomVersionId,
	heads: I,
) -> impl Stream<Item = OwnedEventId> + Send + 'a
where
	I: Iterator<Item = &'a EventId> + Clone + ExactSizeIterator + Send + 'a,
{
	let chain = short_auth_chain(services, room_id, room_version, heads);

	auth_chain_ids(services, chain)
}

async fn short_auth_chain<'a, I>(
	services: &'a Services,
	room_id: &RoomId,
	room_version: &RoomVersionId,
	heads: I,
) -> Vec<ShortEventId>
where
	I: Iterator<Item = &'a EventId> + Clone + ExactSizeIterator + Send + 'a,
{
	services
		.auth_chain
		.get_auth_chain(room_id, room_version, heads)
		.await
		.inspect_err(|error| warn!(%room_id, %error, "Omitting the auth chain of a failed walk."))
		.unwrap_or_default()
}

fn auth_chain_ids<'a, F>(
	services: &'a Services,
	chain: F,
) -> impl Stream<Item = OwnedEventId> + Send + 'a
where
	F: Future + Send + 'a,
	F::Output: IntoIterator<Item = ShortEventId> + Send + 'a,
	<F::Output as IntoIterator>::IntoIter: Send + 'a,
{
	chain
		.map(|chain| {
			services
				.short
				.multi_get_eventid_from_short(chain.stream())
		})
		.flatten_stream()
		.ready_filter_map(Result::ok)
}

/// Reads an auth chain event for a federation response, or omits it.
///
/// An ancestor missing from storage is a normal backfill gap and is omitted
/// quietly, while any other read error is logged first. The rest of the chain
/// still goes out, so one unreadable event never fails the response.
pub(super) async fn auth_chain_pdu(
	services: &Services,
	event_id: &EventId,
	room_version: &RoomVersionId,
) -> Option<Box<RawJsonValue>> {
	services
		.federation
		.get_pdu_into(event_id, Some(room_version))
		.await
		.present()
		.inspect_err(|error| warn!(%event_id, %error, "Omitting unreadable auth event."))
		.ok()
		.flatten()
}

/// Reads a state event for a federation response, or omits it.
///
/// Every event the state names is expected in storage, so any read error, a
/// missing event included, is logged before the event is omitted. The rest of
/// the state still goes out, so one unreadable event never fails the response.
pub(super) async fn state_pdu(
	services: &Services,
	event_id: &EventId,
	room_version: &RoomVersionId,
) -> Option<Box<RawJsonValue>> {
	services
		.federation
		.get_pdu_into(event_id, Some(room_version))
		.await
		.inspect_err(|error| warn!(%event_id, %error, "Omitting unreadable state event."))
		.ok()
}
