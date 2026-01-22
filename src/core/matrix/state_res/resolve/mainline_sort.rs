use std::collections::HashMap;

use futures::{Stream, StreamExt, TryStreamExt, pin_mut};
use ruma::{EventId, OwnedEventId, events::TimelineEventType};

use crate::{
	Result,
	matrix::Event,
	trace,
	utils::stream::{IterStream, TryReadyExt, WidebandExt},
};

/// Perform mainline ordering of the given events.
///
/// Definition in the spec:
/// Given mainline positions calculated from P, the mainline ordering based on P
/// of a set of events is the ordering, from smallest to largest, using the
/// following comparison relation on events: for events x and y, x < y if
///
/// 1. the mainline position of x is greater than the mainline position of y
///    (i.e. the auth chain of x is based on an earlier event in the mainline
///    than y); or
/// 2. the mainline positions of the events are the same, but x’s
///    origin_server_ts is less than y’s origin_server_ts; or
/// 3. the mainline positions of the events are the same and the events have the
///    same origin_server_ts, but x’s event_id is less than y’s event_id.
///
/// ## Arguments
///
/// * `events` - The list of event IDs to sort.
/// * `power_level` - The power level event in the current state.
/// * `fetch_event` - Function to fetch an event in the room given its event ID.
///
/// ## Returns
///
/// Returns the sorted list of event IDs, or an `Err(_)` if one the event in the
/// room has an unexpected format.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		?power_level,
	)
)]
pub(super) async fn mainline_sort<'a, RemainingEvents, Fetch, Fut, Pdu>(
	mut power_level: Option<OwnedEventId>,
	events: RemainingEvents,
	fetch: &Fetch,
) -> Result<Vec<OwnedEventId>>
where
	RemainingEvents: Stream<Item = &'a EventId> + Send,
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	// Populate the mainline of the power level.
	let mut mainline = vec![];
	while let Some(power_level_event_id) = power_level {
		let power_level_event = fetch(power_level_event_id).await?;

		mainline.push(power_level_event.event_id().to_owned());
		power_level = get_power_levels_auth_event(&power_level_event, fetch)
			.await?
			.map(|event| event.event_id().to_owned());
	}

	let mainline_map: HashMap<_, _> = mainline
		.iter()
		.rev()
		.enumerate()
		.map(|(idx, event_id)| (event_id.clone(), idx))
		.collect();

	let order_map: HashMap<_, _> = events
		.wide_filter_map(async |event_id| {
			let event = fetch(event_id.to_owned()).await.ok()?;
			let position = mainline_position(&event, &mainline_map, fetch)
				.await
				.ok()?;

			let event_id = event.event_id().to_owned();
			let origin_server_ts = event.origin_server_ts();
			Some((event_id, (position, origin_server_ts)))
		})
		.collect()
		.await;

	let mut sorted_event_ids: Vec<_> = order_map.keys().cloned().collect();

	sorted_event_ids.sort_by(|a, b| {
		let (a_pos, a_ots) = &order_map[a];
		let (b_pos, b_ots) = &order_map[b];
		a_pos
			.cmp(b_pos)
			.then(a_ots.cmp(b_ots))
			.then(a.cmp(b))
	});

	Ok(sorted_event_ids)
}

/// Get the mainline position of the given event from the given mainline map.
///
/// ## Arguments
///
/// * `event` - The event to compute the mainline position of.
/// * `mainline_map` - The mainline map of the m.room.power_levels event.
/// * `fetch` - Function to fetch an event in the room given its event ID.
///
/// ## Returns
///
/// Returns the mainline position of the event, or an `Err(_)` if one of the
/// events in the auth chain of the event was not found.
#[tracing::instrument(
	level = "trace",
	skip_all,
	fields(
		event = ?event.event_id(),
		mainline = mainline_map.len(),
	)
)]
async fn mainline_position<Fetch, Fut, Pdu>(
	event: &Pdu,
	mainline_map: &HashMap<OwnedEventId, usize>,
	fetch: &Fetch,
) -> Result<usize>
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let mut current_event = Some(event.clone());
	while let Some(event) = current_event {
		trace!(event_id = ?event.event_id(), "mainline");

		// If the current event is in the mainline map, return its position.
		if let Some(position) = mainline_map.get(event.event_id()) {
			return Ok(*position);
		}

		// Look for the power levels event in the auth events.
		current_event = get_power_levels_auth_event(&event, fetch).await?;
	}

	// Did not find a power level event so we default to zero.
	Ok(0)
}

#[expect(clippy::redundant_closure)]
#[tracing::instrument(level = "trace", skip_all)]
async fn get_power_levels_auth_event<Fetch, Fut, Pdu>(
	event: &Pdu,
	fetch: &Fetch,
) -> Result<Option<Pdu>>
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let power_level_event = event
		.auth_events()
		.try_stream()
		.map_ok(ToOwned::to_owned)
		.and_then(|auth_event_id| fetch(auth_event_id))
		.ready_try_skip_while(|auth_event| {
			Ok(!auth_event.is_type_and_state_key(&TimelineEventType::RoomPowerLevels, ""))
		});

	pin_mut!(power_level_event);
	power_level_event.try_next().await
}
