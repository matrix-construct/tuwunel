use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
};

use futures::{StreamExt, TryFutureExt, TryStreamExt};
use ruma::{
	EventId, OwnedEventId,
	events::{TimelineEventType, room::power_levels::UserPowerLevel},
	room_version_rules::RoomVersionRules,
};

use super::{
	super::events::{
		RoomCreateEvent, RoomPowerLevelsEvent, RoomPowerLevelsIntField, is_power_event,
		power_levels::RoomPowerLevelsEventOptionExt,
	},
	AuthSet, topological_sort,
};
use crate::{
	Result, err,
	matrix::Event,
	utils::stream::{BroadbandExt, IterStream, TryBroadbandExt},
};

/// Enlarge the given list of conflicted power events by adding the events in
/// their auth chain that are in the full conflicted set, and sort it using
/// reverse topological power ordering.
///
/// ## Arguments
///
/// * `conflicted_power_events` - The list of power events in the full
///   conflicted set.
///
/// * `full_conflicted_set` - The full conflicted set.
///
/// * `rules` - The authorization rules for the current room version.
///
/// * `fetch` - Function to fetch an event in the room given its event ID.
///
/// ## Returns
///
/// Returns the ordered list of event IDs from earliest to latest.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		full_conflicted = full_conflicted_set.len(),
	)
)]
pub(super) async fn power_sort<Fetch, Fut, Pdu>(
	rules: &RoomVersionRules,
	full_conflicted_set: &AuthSet<OwnedEventId>,
	fetch: &Fetch,
) -> Result<Vec<OwnedEventId>>
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	// A representation of the DAG, a map of event ID to its list of auth events
	// that are in the full conflicted set. Fill the graph.
	let graph = full_conflicted_set
		.iter()
		.stream()
		.broad_filter_map(async |id| is_power_event_id(id, fetch).await.then_some(id))
		.fold(HashMap::new(), |graph, event_id| {
			add_event_auth_chain(graph, full_conflicted_set, event_id, fetch)
		})
		.await;

	// The map of event ID to the power level of the sender of the event.
	// Get the power level of the sender of each event in the graph.
	let event_to_power_level: HashMap<_, _> = graph
		.keys()
		.try_stream()
		.map_ok(AsRef::as_ref)
		.broad_and_then(|event_id| {
			power_level_for_sender(event_id, rules, fetch)
				.map_ok(move |sender_power| (event_id, sender_power))
				.map_err(|e| err!(Request(NotFound("Missing PL for sender: {e}"))))
		})
		.try_collect()
		.await?;

	let query = async |event_id: OwnedEventId| {
		let power_level = *event_to_power_level
			.get(&event_id.borrow())
			.ok_or_else(|| err!(Request(NotFound("Missing PL event: {event_id}"))))?;

		let event = fetch(event_id).await?;
		Ok((power_level, event.origin_server_ts()))
	};

	topological_sort(&graph, &query).await
}

/// Add the event with the given event ID and all the events in its auth chain
/// that are in the full conflicted set to the graph.
#[tracing::instrument(
	name = "auth_chain",
	level = "trace",
	skip_all,
	fields(
		?event_id,
		graph = graph.len(),
	)
)]
async fn add_event_auth_chain<Fetch, Fut, Pdu>(
	mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>>,
	full_conflicted_set: &AuthSet<OwnedEventId>,
	event_id: &EventId,
	fetch: &Fetch,
) -> HashMap<OwnedEventId, HashSet<OwnedEventId>>
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let mut state = vec![event_id.to_owned()];

	// Iterate through the auth chain of the event.
	while let Some(event_id) = state.pop() {
		// Iterate through the auth events of this event.
		let event = fetch(event_id.clone()).await.ok();

		// Add the current event to the graph.
		graph.entry(event_id).or_default();

		let auth_events = event
			.as_ref()
			.map(Event::auth_events)
			.into_iter()
			.flatten();

		for auth_event_id in auth_events {
			// If the auth event ID is in the full conflicted set…
			if !full_conflicted_set.contains(auth_event_id) {
				continue;
			}

			// If the auth event ID is not in the graph, we need to check its auth events
			// later.
			if !graph.contains_key(auth_event_id) {
				state.push(auth_event_id.to_owned());
			}

			let event_id = event
				.as_ref()
				.expect("event is Some if there are auth_events")
				.event_id();

			// Add the auth event ID to the list of incoming edges.
			graph
				.get_mut(event_id)
				.expect("event_id must be added to graph")
				.insert(auth_event_id.to_owned());
		}
	}

	graph
}

/// Find the power level for the sender of the event of the given event ID or
/// return a default value of zero.
///
/// We find the most recent `m.room.power_levels` by walking backwards in the
/// auth chain of the event.
///
/// Do NOT use this anywhere but topological sort.
///
/// ## Arguments
///
/// * `event_id` - The event ID of the event to get the power level of the
///   sender of.
///
/// * `rules` - The authorization rules for the current room version.
///
/// * `fetch` - Function to fetch an event in the room given its event ID.
///
/// ## Returns
///
/// Returns the power level of the sender of the event or an `Err(_)` if one of
/// the auth events if malformed.
#[tracing::instrument(
	name = "sender_power",
	level = "trace",
	skip_all,
	fields(
		?event_id,
	)
)]
async fn power_level_for_sender<Fetch, Fut, Pdu>(
	event_id: &EventId,
	rules: &RoomVersionRules,
	fetch: &Fetch,
) -> Result<UserPowerLevel>
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let mut room_create_event = None;
	let mut room_power_levels_event = None;
	let event = fetch(event_id.to_owned()).await;
	if let Ok(event) = &event
		&& rules
			.authorization
			.room_create_event_id_as_room_id
	{
		let create_id = event.room_id().as_event_id()?;
		let fetched = fetch(create_id).await?;
		room_create_event = Some(RoomCreateEvent::new(fetched));
	}

	for auth_event_id in event
		.as_ref()
		.map(Event::auth_events)
		.into_iter()
		.flatten()
	{
		if let Ok(auth_event) = fetch(auth_event_id.to_owned()).await {
			if auth_event.is_type_and_state_key(&TimelineEventType::RoomPowerLevels, "") {
				room_power_levels_event = Some(RoomPowerLevelsEvent::new(auth_event));
			} else if !rules
				.authorization
				.room_create_event_id_as_room_id
				&& auth_event.is_type_and_state_key(&TimelineEventType::RoomCreate, "")
			{
				room_create_event = Some(RoomCreateEvent::new(auth_event));
			}

			if room_power_levels_event.is_some() && room_create_event.is_some() {
				break;
			}
		}
	}

	let auth_rules = &rules.authorization;
	let creators = room_create_event
		.as_ref()
		.and_then(|event| event.creators(auth_rules).ok());

	if let Some((event, creators)) = event.ok().zip(creators) {
		room_power_levels_event.user_power_level(event.sender(), creators, auth_rules)
	} else {
		room_power_levels_event
			.get_as_int_or_default(RoomPowerLevelsIntField::UsersDefault, auth_rules)
			.map(Into::into)
	}
}

/// Whether the given event ID belongs to a power event.
///
/// See the docs of `is_power_event()` for the definition of a power event.
#[tracing::instrument(
	name = "is_power_event",
	level = "trace",
	skip_all,
	fields(
		?event_id,
	)
)]
async fn is_power_event_id<Fetch, Fut, Pdu>(event_id: &EventId, fetch: &Fetch) -> bool
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	match fetch(event_id.to_owned()).await {
		| Ok(state) => is_power_event(&state),
		| _ => false,
	}
}
