use std::{collections::HashMap, iter::once, time::Duration};

use futures::{
	FutureExt, StreamExt,
	stream::{FuturesOrdered, FuturesUnordered},
};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId,
	RoomId, RoomVersionId, ServerName, int, uint,
};
use serde_json::value::RawValue as RawJsonValue;
use tokio::time::{Instant, timeout_at};
use tuwunel_core::{
	Result, debug_warn, err, implement,
	matrix::{
		Event, PduEvent,
		event::gen_event_id,
		pdu::{MAX_PREV_EVENTS, check_room_id},
	},
	utils::{
		BoolExt,
		stream::{IterStream, automatic_width},
	},
};

use crate::{
	fetcher::{EventWindow, Op, Opts},
	rooms::state_res::topological_sort,
};

#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%origin,
		events = %initial_set.clone().count(),
	),
)]
#[expect(clippy::type_complexity, clippy::too_many_arguments)]
pub(super) async fn fetch_prev<'a, Events>(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	incoming_event_id: &EventId,
	initial_set: Events,
	room_version: &RoomVersionId,
	recursion_level: usize,
	first_ts_in_room: MilliSecondsSinceUnixEpoch,
) -> Result<(Vec<OwnedEventId>, HashMap<OwnedEventId, (PduEvent, CanonicalJsonObject)>)>
where
	Events: Iterator<Item = &'a EventId> + Clone + Send,
{
	let has_gap = initial_set
		.clone()
		.stream()
		.any(async |event_id| !self.services.timeline.pdu_exists(event_id).await)
		.await;

	let wait_ms = self.services.server.config.fetch_prev_wait_ms;
	let has_gap = (has_gap && wait_ms > 0)
		.then_async(|| self.await_prev_gap(initial_set.clone(), Duration::from_millis(wait_ms)))
		.await
		.unwrap_or(has_gap);

	has_gap
		.then_async(|| {
			self.prefetch_missing_events(
				origin,
				room_id,
				incoming_event_id,
				room_version,
				recursion_level,
			)
		})
		.await;

	let mut todo_outlier_stack: FuturesOrdered<_> = initial_set
		.stream()
		.map(ToOwned::to_owned)
		.filter_map(async |event_id| {
			self.services
				.timeline
				.non_outlier_pdu_exists(&event_id)
				.await
				.is_err()
				.then_some(event_id)
		})
		.map(async |event_id| {
			let events = once(event_id.as_ref());
			let auth = self
				.fetch_auth(origin, room_id, events, room_version, recursion_level)
				.await;

			(event_id, auth)
		})
		.map(FutureExt::boxed) // heterogeneous FuturesOrdered
		.collect()
		.await;

	let mut amount = 0;
	let mut eventid_info = HashMap::new();
	let mut graph: HashMap<OwnedEventId, _> = HashMap::with_capacity(todo_outlier_stack.len());
	while let Some((prev_event_id, mut outlier)) = todo_outlier_stack.next().await {
		self.services.server.check_running()?;

		let Some((pdu, mut json_opt)) = outlier.pop() else {
			// Fetch and handle failed
			graph.insert(prev_event_id.clone(), Default::default());
			continue;
		};

		check_room_id(&pdu, room_id)?;

		let limit = self.services.server.config.max_fetch_prev_events;
		if amount > limit {
			debug_warn!(?limit, "Max prev event limit reached!");
			graph.insert(prev_event_id.clone(), Default::default());
			continue;
		}

		if json_opt.is_none() {
			json_opt = self
				.services
				.timeline
				.get_outlier_pdu_json(&prev_event_id)
				.await
				.ok();
		}

		let Some(json) = json_opt else {
			// Get json failed, so this was not fetched over federation
			graph.insert(prev_event_id.clone(), Default::default());
			continue;
		};

		if pdu.origin_server_ts() > first_ts_in_room {
			amount = amount.saturating_add(1);
			debug_assert!(
				pdu.prev_events().count() <= MAX_PREV_EVENTS,
				"PduEvent {prev_event_id} has too many prev_events"
			);

			for prev_prev in pdu.prev_events() {
				if graph.contains_key(prev_prev) {
					continue;
				}

				let prev_prev = prev_prev.to_owned();
				let fetch = async move {
					let fetch = self
						.fetch_auth(
							origin,
							room_id,
							once(prev_prev.as_ref()),
							room_version,
							recursion_level,
						)
						.await;

					(prev_prev, fetch)
				};

				todo_outlier_stack.push_back(fetch.boxed()); // heterogeneous FuturesOrdered
			}

			graph.insert(
				prev_event_id.clone(),
				pdu.prev_events().map(ToOwned::to_owned).collect(),
			);
		} else {
			// Time based check failed
			graph.insert(prev_event_id.clone(), Default::default());
		}

		eventid_info.insert(prev_event_id.clone(), (pdu, json));
	}

	let event_fetch = async |event_id: OwnedEventId| {
		let origin_server_ts = eventid_info
			.get(&event_id)
			.map_or_else(|| uint!(0), |info| info.0.origin_server_ts().get());

		// This return value is the key used for sorting events,
		// events are then sorted by power level, time,
		// and lexically by event_id.
		Ok((int!(0).into(), MilliSecondsSinceUnixEpoch(origin_server_ts)))
	};

	let graph_len = graph.len();
	let sorted = topological_sort(graph, &event_fetch)
		.await
		.map_err(|e| err!(Database(error!("Error sorting prev events: {e}"))))?;

	debug_assert_eq!(
		sorted.len(),
		graph_len,
		"topological sort returned a different number of outputs than inputs"
	);

	debug_assert!(
		sorted.len() >= eventid_info.len(),
		"returned topologically sorted events differ from eventid_info"
	);

	Ok((sorted, eventid_info))
}

#[implement(super::Service)]
async fn await_prev_gap<'a, Events>(&self, initial_set: Events, wait: Duration) -> bool
where
	Events: Iterator<Item = &'a EventId> + Send,
{
	let deadline = Instant::now()
		.checked_add(wait)
		.expect("wait deadline overflows");

	// Each watcher registers before its existence recheck, so a prev that
	// arrives during the recheck still wakes us.
	let pending: FuturesUnordered<_> = initial_set
		.map(|event_id| (event_id, self.services.timeline.watch_event(event_id)))
		.stream()
		.filter_map(async |(event_id, watcher)| {
			(!self.services.timeline.pdu_exists(event_id).await).then_some(watcher)
		})
		.collect()
		.await;

	if pending.is_empty() {
		return false;
	}

	timeout_at(deadline, pending.count())
		.await
		.is_err()
}

/// Fill the prev gap below `incoming_event_id` with one `/get_missing_events`
/// batch, landing each returned event as a local outlier so the per-event walk
/// resolves it without a federation fetch. `latest_events` is the held event
/// the server walks back from, bounded by our forward extremities so it returns
/// only the gap; best effort, so a failed batch or rejected event just leaves
/// that id for the walk.
#[implement(super::Service)]
#[tracing::instrument(name = "missing", level = "debug", skip_all)]
async fn prefetch_missing_events(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	incoming_event_id: &EventId,
	room_version: &RoomVersionId,
	recursion_level: usize,
) {
	let boundary: EventWindow = self
		.services
		.state
		.get_forward_extremities(room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let opts = Opts::new(Op::MissingEvents, room_id.to_owned())
		.latest_events([incoming_event_id.to_owned()])
		.earliest_events(boundary)
		.hint(origin.to_owned())
		.room_version(room_version.to_owned())
		.attempt_limit(super::EVENT_FETCH_ATTEMPT_LIMIT)
		.fanout_for_op();

	let Ok(outcome) = self.services.fetcher.fetch(opts).await else {
		return;
	};

	let Ok(events) = serde_json::from_slice::<Vec<Box<RawJsonValue>>>(&outcome.bytes) else {
		return;
	};

	events
		.into_iter()
		.stream()
		.for_each_concurrent(automatic_width(), async |pdu| {
			self.land_missing_event(origin, room_id, &pdu, room_version, recursion_level)
				.await
				.ok();
		})
		.await;
}

/// Authenticate and persist one event from the missing-events batch as an
/// outlier, deriving its id from content rather than trusting a requested id.
#[implement(super::Service)]
#[tracing::instrument(name = "land", level = "trace", skip_all)]
async fn land_missing_event(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	pdu: &RawJsonValue,
	room_version: &RoomVersionId,
	recursion_level: usize,
) -> Result {
	let value: CanonicalJsonObject = serde_json::from_str(pdu.get())
		.map_err(|e| err!(BadServerResponse("missing-events pdu is not canonical json: {e}")))?;

	value
		.get("room_id")
		.and_then(CanonicalJsonValue::as_str)
		.is_some_and(|id| id == room_id.as_str())
		.then_some(())
		.ok_or_else(|| {
			err!(Request(InvalidParam("missing-events pdu is for a different room")))
		})?;

	let event_id = gen_event_id(&value, room_version)?;

	// cold arm: missing events
	Box::pin(self.handle_outlier_pdu(
		origin,
		room_id,
		&event_id,
		value,
		room_version,
		recursion_level,
		false,
	))
	.await
	.map(|_| ())
}
