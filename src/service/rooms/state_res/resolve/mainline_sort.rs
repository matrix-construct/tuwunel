use std::collections::HashMap;

use futures::{Stream, StreamExt, TryFutureExt, TryStreamExt, stream::try_unfold};
use ruma::{EventId, OwnedEventId, events::TimelineEventType};
use tuwunel_core::{
	Error, Result, at,
	matrix::{Event, PduEvent, event_id::RandomState},
	result::NotFound,
	trace,
	utils::stream::{BroadbandExt, TryReadyExt},
};

use super::super::FetchEvent;

/// Mainline position of each power-levels event, oldest first.
type Positions<'a> = HashMap<&'a EventId, usize, RandomState>;

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
		power_levels = power_level_event_id
			.as_deref()
			.map(EventId::as_str)
			.unwrap_or_default(),
	)
)]
pub(super) async fn mainline_sort<'a, RemainingEvents>(
	power_level_event_id: Option<OwnedEventId>,
	events: RemainingEvents,
	fetch: impl FetchEvent,
) -> Result<Vec<OwnedEventId>>
where
	RemainingEvents: Stream<Item = &'a EventId> + Send,
{
	// Populate the mainline of the power level.
	let mainline: Vec<_> = try_unfold(power_level_event_id, async |power_level_event_id| {
		let Some(power_level_event_id) = power_level_event_id else {
			return Ok::<_, Error>(None);
		};

		let power_level_event = fetch
			.get::<PduEvent>(&power_level_event_id)
			.await?;

		let this_event_id = power_level_event.event_id().to_owned();
		let next_event_id = get_power_levels_auth_event(&power_level_event, fetch)
			.map_ok(|event| {
				event
					.as_ref()
					.map(Event::event_id)
					.map(ToOwned::to_owned)
			})
			.await?;

		trace!(?this_event_id, ?next_event_id, "mainline descent",);

		Ok(Some((this_event_id, next_event_id)))
	})
	.try_collect()
	.await?;

	let positions: Positions<'_> = mainline
		.iter()
		.rev()
		.map(AsRef::as_ref)
		.enumerate()
		.map(|(position, event_id)| (event_id, position))
		.collect();

	events
		.map(ToOwned::to_owned)
		.broad_then(async |event_id| {
			let Some(event) = fetch
				.get::<PduEvent>(&event_id)
				.await
				.optional()?
			else {
				return Ok(None);
			};

			let origin_server_ts = event.origin_server_ts();
			let Some(position) = mainline_position(Some(event), &positions, fetch)
				.await
				.optional()?
			else {
				return Ok(None);
			};

			Ok(Some((event_id, (position, origin_server_ts))))
		})
		.ready_try_filter_map(Result::Ok)
		.inspect_ok(|(event_id, (position, origin_server_ts))| {
			trace!(position, ?origin_server_ts, ?event_id, "mainline position");
		})
		.try_collect()
		.map_ok(|mut events: Vec<_>| {
			events.sort_by(|a, b| {
				let (a_pos, a_ots) = &a.1;
				let (b_pos, b_ots) = &b.1;
				a_pos
					.cmp(b_pos)
					.then(a_ots.cmp(b_ots))
					.then(a.cmp(b))
			});

			events.into_iter().map(at!(0)).collect()
		})
		.await
}

/// Get the mainline position of the given event from the given mainline map.
///
/// ## Arguments
///
/// * `event` - The event to compute the mainline position of.
/// * `positions` - The mainline positions of the m.room.power_levels events.
/// * `fetch` - Function to fetch an event in the room given its event ID.
///
/// ## Returns
///
/// Returns the mainline position of the event, or an `Err(_)` if one of the
/// events in the auth chain of the event was not found.
#[tracing::instrument(
	name = "position",
	level = "trace",
	ret(level = "trace"),
	skip_all,
	fields(
		mainline = positions.len(),
		event = ?current_event.as_ref().map(Event::event_id).map(ToOwned::to_owned),
	)
)]
async fn mainline_position(
	mut current_event: Option<PduEvent>,
	positions: &Positions<'_>,
	fetch: impl FetchEvent,
) -> Result<usize> {
	while let Some(event) = current_event {
		trace!(
			event_id = ?event.event_id(),
			"mainline position search",
		);

		// Real positions are 1..N (i + 1) so that 0 is free to mark
		// "no power-levels in the auth chain". Without that, no-PL events
		// would tie with events rooted at the oldest mainline PL.
		if let Some(position) = positions.get(event.event_id()) {
			return Ok(position.saturating_add(1));
		}

		// Look for the power levels event in the auth events.
		current_event = get_power_levels_auth_event(&event, fetch).await?;
	}

	// No power-levels ancestor in the auth chain; sort before all
	// chain-rooted events.
	Ok(0)
}

#[tracing::instrument(level = "trace", skip_all)]
async fn get_power_levels_auth_event(
	event: &PduEvent,
	fetch: impl FetchEvent,
) -> Result<Option<PduEvent>> {
	// A stream adapter cannot satisfy the borrowed fetch future's higher-ranked bound.
	for auth_event_id in event.auth_events() {
		let auth_event: PduEvent = fetch.get(auth_event_id).await?;

		if auth_event.is_type_and_state_key(&TimelineEventType::RoomPowerLevels, "") {
			return Ok(Some(auth_event));
		}
	}

	Ok(None)
}
