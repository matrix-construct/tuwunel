use futures::{Stream, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{
	EventId, OwnedEventId,
	events::{StateEventType, TimelineEventType},
	room_version_rules::RoomVersionRules,
};

use super::{
	super::{auth_types_for_event, check_state_dependent_auth_rules},
	StateMap,
};
use crate::{
	Error, Result, debug_warn, err, error,
	matrix::{Event, EventTypeExt, StateKey},
	trace,
	utils::stream::{IterStream, ReadyExt, TryReadyExt, TryWidebandExt},
};

/// Perform the iterative auth checks to the given list of events.
///
/// Definition in the specification:
///
/// The iterative auth checks algorithm takes as input an initial room state and
/// a sorted list of state events, and constructs a new room state by iterating
/// through the event list and applying the state event to the room state if the
/// state event is allowed by the authorization rules. If the state event is not
/// allowed by the authorization rules, then the event is ignored. If a
/// (event_type, state_key) key that is required for checking the authorization
/// rules is not present in the state, then the appropriate state event from the
/// event’s auth_events is used if the auth event is not rejected.
///
/// ## Arguments
///
/// * `rules` - The authorization rules for the current room version.
/// * `events` - The sorted state events to apply to the `partial_state`.
/// * `state` - The current state that was partially resolved for the room.
/// * `fetch_event` - Function to fetch an event in the room given its event ID.
///
/// ## Returns
///
/// Returns the partially resolved state, or an `Err(_)` if one of the state
/// events in the room has an unexpected format.
#[tracing::instrument(
	name = "iterative_auth",
	level = "debug",
	skip_all,
	fields(
		states = ?state.len(),
	)
)]
pub(super) async fn iterative_auth_check<'b, SortedPowerEvents, Fetch, Fut, Pdu>(
	rules: &RoomVersionRules,
	events: SortedPowerEvents,
	state: StateMap<OwnedEventId>,
	fetch: &Fetch,
) -> Result<StateMap<OwnedEventId>>
where
	SortedPowerEvents: Stream<Item = &'b EventId> + Send,
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	events
		.map(Ok)
		.wide_and_then(async |event_id| {
			let event = fetch(event_id.to_owned()).await?;
			let state_key: StateKey = event
				.state_key()
				.ok_or_else(|| err!(Request(InvalidParam("Missing state_key"))))?
				.into();

			Ok((event_id, state_key, event))
		})
		.try_fold(state, |state, (event_id, state_key, event)| {
			auth_check(rules, state, event_id, state_key, event, fetch)
		})
		.await
}

#[tracing::instrument(
	name = "check",
	level = "trace",
	skip_all,
	fields(
		%event_id,
		?state_key,
	)
)]
async fn auth_check<Fetch, Fut, Pdu>(
	rules: &RoomVersionRules,
	mut state: StateMap<OwnedEventId>,
	event_id: &EventId,
	state_key: StateKey,
	event: Pdu,
	fetch: &Fetch,
) -> Result<StateMap<OwnedEventId>>
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let Ok(auth_types) = auth_types_for_event(
		event.event_type(),
		event.sender(),
		Some(&state_key),
		event.content(),
		&rules.authorization,
		true,
	)
	.inspect_err(|e| error!("failed to get auth types for event: {e}")) else {
		return Ok(state);
	};

	let auth_types_events = auth_types
		.stream()
		.ready_filter_map(|key| {
			state
				.get(&key)
				.map(move |auth_event_id| (auth_event_id, key))
		})
		.filter_map(async |(id, key)| {
			fetch(id.clone())
				.inspect_err(|e| debug_warn!(%id, "missing auth event: {e}"))
				.inspect_err(|e| debug_assert!(!cfg!(test), "missing auth {id:?}: {e:?}"))
				.map_ok(move |auth_event| (key, auth_event))
				.await
				.ok()
		})
		.ready_filter_map(|(key, auth_event)| {
			auth_event
				.rejected()
				.eq(&false)
				.then_some((key, auth_event))
		})
		.map(Ok);

	// If the `m.room.create` event is not in the auth events, we need to add it,
	// because it's always part of the state and required in the auth rules.
	let also_need_create_event = *event.event_type() != TimelineEventType::RoomCreate
		&& rules
			.authorization
			.room_create_event_id_as_room_id;

	let also_create_id: Option<OwnedEventId> = also_need_create_event
		.then(|| event.room_id().as_event_id().ok())
		.flatten();

	let auth_events = event
		.auth_events()
		.chain(also_create_id.as_deref().into_iter())
		.stream()
		.filter_map(async |id| {
			fetch(id.to_owned())
				.inspect_err(|e| debug_warn!(%id, "missing auth event: {e}"))
				.inspect_err(|e| debug_assert!(!cfg!(test), "missing auth {id:?}: {e:?}"))
				.await
				.ok()
		})
		.map(Result::<Pdu, Error>::Ok)
		.ready_try_filter_map(|auth_event| {
			let state_key = auth_event
				.state_key()
				.ok_or_else(|| err!(Request(InvalidParam("Missing state_key"))))?;

			let key_val = auth_event
				.rejected()
				.eq(&false)
				.then_some((auth_event.event_type().with_state_key(state_key), auth_event));

			Ok(key_val)
		});

	let auth_events: Vec<_> = auth_events
		.chain(auth_types_events)
		.try_collect()
		.map_ok(|mut vec: Vec<_>| {
			vec.sort_by(|a, b| a.0.cmp(&b.0));
			vec.reverse();
			vec.dedup_by(|a, b| a.0.eq(&b.0));
			vec
		})
		.await?;

	let fetch_state = async |ty: StateEventType, key: StateKey| -> Result<Pdu> {
		trace!(?ty, ?key, auth_events = auth_events.len(), "fetch state");
		auth_events
			.binary_search_by(|a| ty.cmp(&a.0.0).then(key.cmp(&a.0.1)))
			.map(|i| auth_events[i].1.clone())
			.map_err(|_| err!(Request(NotFound("Missing auth_event {ty:?},{key:?}"))))
	};

	// Add authentic event to the partially resolved state.
	if check_state_dependent_auth_rules(rules, &event, &fetch_state)
		.await
		.inspect_err(|e| debug_warn!("event failed auth check: {e}"))
		.is_ok()
	{
		let key = event.event_type().with_state_key(state_key);
		state.insert(key, event_id.to_owned());
	}

	Ok(state)
}
