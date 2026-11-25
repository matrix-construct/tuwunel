use std::{
	collections::{HashMap, VecDeque},
	iter::once,
	num::NonZeroUsize,
	time::Duration,
};

use futures::{
	StreamExt, TryStreamExt,
	stream::{FuturesOrdered, FuturesUnordered},
};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId,
	int, uint,
};
use serde_json::value::RawValue as RawJsonValue;
use tokio::time::{Instant, timeout_at};
use tuwunel_core::{
	Result, err, implement,
	utils::{
		BoolExt,
		stream::{IterStream, automatic_width},
	},
};
use tuwunel_matrix::{
	Event, PduEvent,
	event::gen_event_id,
	pdu::{MAX_PREV_EVENTS, check_room_id},
	room::state::resolution::topological_sort,
};

use super::{eval::Eval, fetch_auth::AuthFetchBudget};
use crate::fetcher::{EventWindow, Op, Opts};

pub(super) type Pdus = HashMap<OwnedEventId, (PduEvent, CanonicalJsonObject)>;

/// Events requested by the `/get_missing_events` batch before the walk.
const MISSING_EVENTS_LIMIT: NonZeroUsize = NonZeroUsize::new(10).unwrap();

/// Fetches one walk runs at once. This covers every prev event of one event,
/// at most 20, and leaves most of the fetcher's 256 slots to other rooms.
const PREV_FETCH_WIDTH: usize = 32;

#[cfg(test)]
mod tests;

struct PrevQueue {
	waiting: VecDeque<OwnedEventId>,
	remaining: usize,
}

impl PrevQueue {
	fn new(waiting: VecDeque<OwnedEventId>, limit: usize) -> Self {
		// Preserve the direct predecessors and the existing limit + 1 boundary.
		let remaining = limit
			.saturating_add(1)
			.saturating_sub(waiting.len());
		Self { waiting, remaining }
	}

	fn admit(&mut self, id: &EventId) -> bool {
		let Some(remaining) = self.remaining.checked_sub(1) else { return false };
		self.remaining = remaining;
		self.waiting.push_back(id.to_owned());
		true
	}

	fn take_ready(&mut self, in_flight: usize) -> impl Iterator<Item = OwnedEventId> + '_ {
		let free = PREV_FETCH_WIDTH
			.saturating_sub(in_flight)
			.min(self.waiting.len());
		self.waiting.drain(..free)
	}
}

/// An incoming event's missing previous events, walked backwards.
///
/// The incoming event's handler upgrades the walked events and records
/// `capped` in the prev-walk counters.
pub(super) struct PrevFetch {
	/// Walked event ids in topological order, including placeholders for events
	/// that failed to fetch.
	pub(super) sorted: Vec<OwnedEventId>,

	/// Fetched PDUs of the walked events.
	pub(super) pdus: Pdus,

	/// Whether the `max_fetch_prev_events` cap cut the walk.
	pub(super) capped: bool,
}

/// Walk an incoming event's missing previous events backwards.
///
/// Each fetched event queues its own missing previous events in turn, until
/// they reach the timeline, predate the room, or the queued events reach the
/// `max_fetch_prev_events` cap.
#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%origin,
		events = %initial_set.clone().count(),
	),
)]
pub(super) async fn fetch_prev<'a, Events>(
	&self,
	eval @ Eval { origin, room_id, .. }: Eval<'_>,
	first_normal_ts: MilliSecondsSinceUnixEpoch,
	initial_set: Events,
	budget: &AuthFetchBudget,
) -> Result<PrevFetch>
where
	Events: Iterator<Item = &'a EventId> + Clone + Send,
{
	let has_gap = initial_set
		.clone()
		.stream()
		.any(async |event_id| !self.services.timeline.pdu_exists(event_id).await)
		.await;

	let wait_ms = self
		.services
		.server
		.config
		.federation
		.history
		.fetch_prev_wait_ms;

	let has_gap = (has_gap && wait_ms > 0)
		.then_async(|| self.await_prev_gap(initial_set.clone(), Duration::from_millis(wait_ms)))
		.await
		.unwrap_or(has_gap);

	has_gap
		.then_async(|| self.prefetch_missing_events(eval, budget))
		.await;

	let waiting: VecDeque<OwnedEventId> = initial_set
		.try_stream()
		.map_ok(ToOwned::to_owned)
		.try_filter_map(async |event_id| {
			// A prev event already in the timeline is not walked below, so its room is
			// checked here instead.
			match self
				.services
				.timeline
				.get_non_outlier_pdu(&event_id)
				.await
			{
				| Ok(pdu) => check_room_id(&pdu, room_id).map(|()| None),
				| Err(_) => Ok(Some(event_id)),
			}
		})
		.try_collect()
		.await?;

	let fetch = async |event_id: OwnedEventId| {
		let events = once(event_id.as_ref());
		let auth = self.fetch_auth(eval, events, budget).await;

		(event_id, auth)
	};

	// Every queued event is fetched in the end, so the cap counts them all, not
	// only the walked ones.
	let mut graph: HashMap<OwnedEventId, _> = HashMap::with_capacity(waiting.len());
	let mut queue = PrevQueue::new(waiting, budget.limit);
	let mut capped = false;
	let mut pdus = HashMap::new();
	let mut todo_outlier_stack = FuturesOrdered::new();

	loop {
		// A finished fetch counts until it is popped, so at most PREV_FETCH_WIDTH
		// run at once.
		let ready = queue.take_ready(todo_outlier_stack.len());
		todo_outlier_stack.extend(ready.map(&fetch));

		let Some((prev_event_id, outlier)) = todo_outlier_stack.next().await else {
			break;
		};

		self.services.server.check_running()?;

		let Some((pdu, mut json_opt)) = outlier?.pop() else {
			// Fetch and handle failed
			graph.insert(prev_event_id.clone(), Default::default());
			continue;
		};

		check_room_id(&pdu, room_id)?;

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

		// handle_prev_pdu upgrades events this old too, so their prev events must be
		// walked and room-checked as well.
		if pdu.origin_server_ts() >= first_normal_ts {
			debug_assert!(
				pdu.prev_events().count() <= MAX_PREV_EVENTS,
				"PduEvent {prev_event_id} has too many prev_events"
			);

			for prev_prev in pdu.prev_events() {
				if graph.contains_key(prev_prev) {
					continue;
				}

				if !queue.admit(prev_prev) {
					capped = true;
					break;
				}
			}

			graph.insert(
				prev_event_id.clone(),
				pdu.prev_events().map(ToOwned::to_owned).collect(),
			);
		} else {
			// Time based check failed
			graph.insert(prev_event_id.clone(), Default::default());
		}

		pdus.insert(prev_event_id.clone(), (pdu, json));
	}

	let event_fetch = async |event_id: OwnedEventId| {
		let origin_server_ts = pdus
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
		sorted.len() >= pdus.len(),
		"returned topologically sorted events differ from pdus"
	);

	Ok(PrevFetch { sorted, pdus, capped })
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
	eval @ Eval {
		origin,
		room_id,
		event_id: incoming_event_id,
		room_version,
		..
	}: Eval<'_>,
	budget: &AuthFetchBudget,
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
		.backfill_limit(MISSING_EVENTS_LIMIT)
		.fanout_for_op();

	let Ok(outcome) = self.services.fetcher.fetch(opts).await else {
		return;
	};

	let Ok(events) = missing_events(&outcome.bytes) else {
		return;
	};

	events
		.stream()
		.for_each_concurrent(automatic_width(), async |pdu| {
			self.land_missing_event(eval, pdu, budget)
				.await
				.ok();
		})
		.await;
}

/// Land at most as many events as the request asked for.
fn missing_events(bytes: &[u8]) -> serde_json::Result<impl Iterator<Item = &RawJsonValue>> {
	let events: Vec<&RawJsonValue> = serde_json::from_slice(bytes)?;
	Ok(events
		.into_iter()
		.take(MISSING_EVENTS_LIMIT.get()))
}

/// Authenticate and persist one event from the missing-events batch as an
/// outlier, deriving its id from content rather than trusting a requested id.
#[implement(super::Service)]
#[tracing::instrument(name = "land", level = "trace", skip_all)]
async fn land_missing_event(
	&self,
	eval @ Eval { room_id, room_version, .. }: Eval<'_>,
	pdu: &RawJsonValue,
	budget: &AuthFetchBudget,
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
	let missing = Eval {
		event_id: &event_id,
		auth_events_known: false,
		..eval
	};

	// cold arm: missing events
	Box::pin(self.handle_outlier_pdu(missing, value, budget))
		.await
		.map(|_| ())
}
