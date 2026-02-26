use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	iter::once,
};

use futures::{StreamExt, TryFutureExt, TryStreamExt, stream::FuturesUnordered};
use ruma::{
	EventId, OwnedEventId,
	events::{TimelineEventType, room::power_levels::UserPowerLevel},
	room_version_rules::RoomVersionRules,
};
use tuwunel_core::{
	Result, err,
	matrix::Event,
	utils::stream::{BroadbandExt, IterStream, TryBroadbandExt},
};

use super::super::{
	events::{
		RoomCreateEvent, RoomPowerLevelsEvent, RoomPowerLevelsIntField, is_power_event,
		power_levels::RoomPowerLevelsEventOptionExt,
	},
	topological_sort,
	topological_sort::ReferencedIds,
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
		conflicted = full_conflicted_set.len(),
	)
)]
pub(super) async fn power_sort<Fetch, Fut, Pdu>(
	rules: &RoomVersionRules,
	full_conflicted_set: &HashSet<OwnedEventId>,
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
		.broad_filter_map(async |id| {
			is_power_event_id(id, fetch)
				.await
				.then(|| id.clone())
		})
		.enumerate()
		.fold(HashMap::new(), |graph, (i, event_id)| {
			add_event_auth_chain(full_conflicted_set, graph, event_id, fetch, i)
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
		graph = graph.len(),
		?event_id,
		%i,
	)
)]
async fn add_event_auth_chain<Fetch, Fut, Pdu>(
	full_conflicted_set: &HashSet<OwnedEventId>,
	mut graph: HashMap<OwnedEventId, ReferencedIds>,
	event_id: OwnedEventId,
	fetch: &Fetch,
	i: usize,
) -> HashMap<OwnedEventId, ReferencedIds>
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let mut todo: FuturesUnordered<Fut> = once(fetch(event_id)).collect();

	while let Some(event) = todo.next().await {
		let Ok(event) = event else {
			continue;
		};

		let event_id = event.event_id().to_owned();
		graph.entry(event_id.clone()).or_default();

		for auth_event_id in event
			.auth_events_into()
			.into_iter()
			.filter(|auth_event_id| full_conflicted_set.contains(auth_event_id))
		{
			if !graph.contains_key(&auth_event_id) {
				todo.push(fetch(auth_event_id.clone()));
			}

			let references = graph
				.get_mut(&event_id)
				.expect("event_id present in graph");

			if !references.contains(&auth_event_id) {
				references.push(auth_event_id);
			}
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
	let event = fetch(event_id.into()).await;
	let hydra_room_id = rules
		.authorization
		.room_create_event_id_as_room_id;

	let mut create_event = None;
	let mut power_levels_event = None;
	if hydra_room_id && let Ok(event) = event.as_ref() {
		let create_id = event.room_id().as_event_id()?;
		let fetched = fetch(create_id).await?;

		_ = create_event.insert(RoomCreateEvent::new(fetched));
	}

	for auth_event_id in event
		.as_ref()
		.map(Event::auth_events)
		.into_iter()
		.flatten()
	{
		use TimelineEventType::{RoomCreate, RoomPowerLevels};

		let Ok(auth_event) = fetch(auth_event_id.to_owned()).await else {
			continue;
		};

		if !hydra_room_id && auth_event.is_type_and_state_key(&RoomCreate, "") {
			_ = create_event.get_or_insert_with(|| RoomCreateEvent::new(auth_event));
		} else if auth_event.is_type_and_state_key(&RoomPowerLevels, "") {
			_ = power_levels_event.get_or_insert_with(|| RoomPowerLevelsEvent::new(auth_event));
		}

		if power_levels_event.is_some() && create_event.is_some() {
			break;
		}
	}

	let creators = create_event
		.as_ref()
		.and_then(|event| event.creators(&rules.authorization).ok());

	if let Some((event, creators)) = event.ok().zip(creators) {
		power_levels_event.user_power_level(event.sender(), creators, &rules.authorization)
	} else {
		power_levels_event
			.get_as_int_or_default(RoomPowerLevelsIntField::UsersDefault, &rules.authorization)
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
