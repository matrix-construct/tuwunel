//! Retrieves older room history from federation and inserts validated events.
//!
//! Candidate servers are selected from trusted and room-related origins, while
//! remote timestamp claims are checked against the ingested event. Backfilled
//! events receive negative stream counts so they sort before normal history.

use std::{
	cmp::Reverse,
	collections::{BTreeSet, HashSet, VecDeque},
	convert::identity,
	iter::once,
	num::NonZeroUsize,
	pin::pin,
};

use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt,
	future::{join, try_join3},
	stream::unfold,
};
use rand::seq::SliceRandom;
use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedServerName,
	RoomId, ServerName, UInt, api::Direction, events::TimelineEventType,
};
use serde::Deserialize;
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{
	Err, Result, at, debug, debug_warn, implement, is_false,
	itertools::Itertools,
	smallvec::SmallVec,
	utils::{
		BoolExt, IterStream, ReadyExt, TryReadyExt,
		future::{BoolExt as FutureBoolExt, TryExtExt},
		result::NotFound,
	},
	validated, warn,
};
use tuwunel_database::Json;
use tuwunel_matrix::{
	PduEvent,
	event::Event,
	pdu::{PduCount, PduId, PrevEvents, RawPduId},
};

use super::{ExtractBody, bias_count};
use crate::{
	federation::Candidates,
	fetcher::{Op, Opts},
	rooms::{event_handler::PolicyCheck, state_accessor::plain_text_topic},
};

#[cfg(test)]
mod tests;

/// Events requested per backfill batch.
const BACKFILL_LIMIT: NonZeroUsize = NonZeroUsize::new(100).unwrap();

const BACKFILL_ATTEMPT_LIMIT: NonZeroUsize = NonZeroUsize::new(5).unwrap();

const BACKFILL_BATCH_ATTEMPTS: usize = 3;

/// What backfill did with one remotely supplied event.
///
/// A deferred event ends ingestion of its batch and the backfill call, so
/// nothing older is prepended past it; a skipped one does not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backfilled {
	/// The event joined the room's history.
	Prepended,

	/// The event was left out, as one accepted already or withheld as
	/// soft-failed.
	Skipped,

	/// The room's policy server gave no verdict on the event, which stays an
	/// unmarked outlier until backfill offers it again.
	Deferred,
}

/// The `event_id` and timestamp parsed back out of an [`Op::TimestampToEvent`]
/// fetch outcome.
#[derive(Deserialize)]
struct TimestampHit {
	event_id: OwnedEventId,
	origin_server_ts: MilliSecondsSinceUnixEpoch,
}

/// The fields of a backfilled event that place it in the ingestion order.
///
/// Only these two are read, so the rest of the event is skipped over.
#[derive(Deserialize)]
struct Placement {
	#[serde(rename = "type")]
	kind: TimelineEventType,

	depth: UInt,
}

/// One backfill request.
///
/// It holds where the request starts, the candidates still left for the room's
/// oldest event, the frontier seeds already asked, and the counted requests
/// left.
#[cfg_attr(test, derive(Debug, PartialEq))]
struct Attempt {
	seed: Seed,
	anchor: Candidates,
	asked: Passed,
	remaining: usize,
}

/// Where a backfill request starts.
///
/// The first move off the oldest event is the call's only uncounted request.
/// Once the call has fallen back to the oldest event, frontier seeds not yet
/// asked wait until no candidate is left for it.
#[cfg_attr(test, derive(Debug, PartialEq))]
enum Seed {
	/// The room's oldest event, before the seed walk has moved.
	Oldest,

	/// Another event the walk reached, asked of every candidate: a withheld one,
	/// or the oldest event still missing history.
	Frontier(OwnedEventId),

	/// The room's oldest event again, asked of the candidates left for it.
	Fallback,
}

type Passed = BTreeSet<OwnedEventId>;

type Pending = VecDeque<(OwnedEventId, PrevEvents)>;

type Frontier = SmallVec<[OwnedEventId; 1]>;

type Unheld = SmallVec<[OwnedEventId; 1]>;

/// The outcome of one backfill request.
///
/// From the first request `Empty` and `Prepended` can lead to another, and from
/// any later one every outcome but `Deferred` can.
enum Batch {
	/// No candidate answered.
	Unanswered,

	/// At least one event was prepended.
	Prepended,

	/// The origin answered, and nothing was prepended.
	Empty(OwnedServerName),

	/// An event was deferred, so the rest of the batch and the call wait for a
	/// later request.
	Deferred {
		/// Whether events ahead of it were prepended.
		prepended: bool,
	},
}

/// Attempts federation backfill when a request reaches local history's edge.
///
/// Backfill is skipped after the create event and for effectively empty rooms
/// that are not world-readable. The first request asks from the room's oldest
/// event. Before it, a scan finds the oldest event still missing history, which
/// lies above the first once a later batch left that one's history whole; after
/// each batch the frontier is walked from the first event and then from that
/// one, and its events are asked in turn while requests are left, every branch
/// of a withheld fork even after a prepend. A first batch that prepends nothing
/// is otherwise asked again of the remaining candidates, and a seed that cannot
/// progress returns to the oldest event first. An event the room's policy
/// server gives no verdict on ends the call, so nothing older is prepended past
/// it. No candidate, a remote fetch failure, or an empty accepted chunk is
/// treated as a successful best-effort outcome.
#[implement(super::Service)]
#[tracing::instrument(name = "backfill", level = "debug", skip(self))]
pub async fn backfill_if_required(&self, room_id: &RoomId, from: PduCount) -> Result {
	let (first_pdu_count, first_pdu) = self.first_item_in_room(room_id).await?;

	if first_pdu_count < from {
		return Ok(());
	}

	// No backfill required, reached the end.
	if *first_pdu.event_type() == TimelineEventType::RoomCreate {
		return Ok(());
	}

	let empty_room = self
		.services
		.state_cache
		.room_joined_count(room_id)
		.map_ok_or(true, |count| count <= 1);

	let not_world_readable = self
		.services
		.state_accessor
		.is_world_readable(room_id)
		.map(is_false!());

	// Room is empty (1 user or none), there is no one that can backfill
	if empty_room.and(not_world_readable).await {
		return Ok(());
	}

	let eligible = self.backfill_candidates(room_id).await;

	let no_backfill = || {
		warn!(%room_id, "No servers could backfill, but backfill was needed");
		Ok(())
	};

	// Empty here, rather than deferring to the fetcher, keeps backfill scoped to
	// the authoritative servers; the fetcher would otherwise fall back to the
	// room's whole population.
	if eligible.is_empty() {
		return no_backfill();
	}

	// The first event's own walk already covers it when the scan stops there.
	let scanned = pin!(self.backfill_edges(room_id))
		.next()
		.map(|edge| edge.filter(|edge| edge.event_id != first_pdu.event_id))
		.await;

	// The oldest event goes first: a responder holding a withheld seed only as an
	// outlier answers it with nothing.
	let first = Attempt {
		seed: Seed::Oldest,
		anchor: eligible.clone(),
		asked: Passed::new(),
		remaining: BACKFILL_BATCH_ATTEMPTS,
	};

	let attempts = unfold(Some(first), async |attempt| {
		let attempt: Attempt = attempt?;
		let (event_id, candidates) = match &attempt.seed {
			| Seed::Frontier(event_id) => (event_id.as_ref(), &eligible),
			| Seed::Oldest | Seed::Fallback => (first_pdu.event_id(), &attempt.anchor),
		};

		let batch = self
			.backfill_batch(room_id, event_id, candidates)
			.await;

		let next = match &batch {
			| Ok(batch) if walks(&attempt, batch) => {
				// Walked again after each batch, so withheld events the batch stored
				// extend the frontier.
				let frontier: Frontier = self
					.backfill_seeds(room_id, &first_pdu, scanned.as_ref())
					.collect()
					.boxed() // size firewall
					.await;

				retry(attempt, batch, first_pdu.event_id(), &frontier)
			},
			| _ => None,
		};

		Some((batch, next))
	});

	let (prepended, last) = attempts
		.ready_fold((false, None), |(prepended, _), batch| {
			let prepends =
				matches!(batch, Ok(Batch::Prepended | Batch::Deferred { prepended: true }));

			(prepended || prepends, Some(batch))
		})
		.await;

	match (prepended, last) {
		| (_, Some(Err(error))) => Err(error),
		| (true, _) => Ok(()),
		| (false, Some(Ok(Batch::Unanswered))) => no_backfill(),
		| (false, _) => {
			warn!(%room_id, "Backfill was required but prepended no events");
			Ok(())
		},
	}
}

#[implement(super::Service)]
async fn backfill_candidates(&self, room_id: &RoomId) -> Candidates {
	let canonical_alias = self
		.services
		.state_accessor
		.get_canonical_alias(room_id);

	let power_levels = self
		.services
		.state_accessor
		.get_power_levels(room_id);

	let (canonical_alias, power_levels) = join(canonical_alias, power_levels).await;

	let power_servers = power_levels
		.iter()
		.flat_map(|power| {
			power
				.rules
				.privileged_creators
				.iter()
				.flat_map(|creators| creators.iter())
		})
		.chain(power_levels.iter().flat_map(|power| {
			power
				.users
				.iter()
				.filter_map(|(user_id, level)| level.gt(&power.users_default).then_some(user_id))
		}))
		.filter_map(|user_id| {
			self.services
				.globals
				.user_is_local(user_id)
				.is_false()
				.then_some(user_id.server_name())
		})
		.collect::<HashSet<_>>();

	let power_servers = {
		let mut vec: Vec<_> = power_servers
			.into_iter()
			.map(ToOwned::to_owned)
			.collect();

		vec.shuffle(&mut rand::rng());
		vec.into_iter().stream()
	};

	let canonical_room_alias_server = once(canonical_alias)
		.filter_map(Result::ok)
		.map(|alias| alias.server_name().to_owned())
		.stream();

	let trusted_servers = self
		.services
		.server
		.config
		.federation
		.keys
		.trusted_servers
		.iter()
		.map(ToOwned::to_owned)
		.stream();

	power_servers
		.chain(canonical_room_alias_server)
		.chain(trusted_servers)
		.ready_filter(|server_name| !self.services.globals.server_is_ours(server_name))
		.filter_map(async |server_name| {
			self.services
				.state_cache
				.server_in_room(&server_name, room_id)
				.await
				.then_some(server_name)
		})
		.collect()
		.await
}

/// Asks the candidates for the events before `seed` and ingests the batch.
///
/// A fetch no candidate answers is `Unanswered` and an unparsable response is
/// an error, while each event's own ingestion failure only leaves it out of the
/// count. Ingestion stops at the first deferred event.
#[implement(super::Service)]
#[tracing::instrument(skip(self, eligible), level = "debug")]
async fn backfill_batch(
	&self,
	room_id: &RoomId,
	seed: &EventId,
	eligible: &Candidates,
) -> Result<Batch> {
	let opts = Opts::new(Op::Backfill, room_id.to_owned())
		.event_id(seed.to_owned())
		.candidates(eligible.iter().cloned())
		.attempt_limit(BACKFILL_ATTEMPT_LIMIT)
		.backfill_limit(BACKFILL_LIMIT);

	let Ok(outcome) = self.services.fetcher.fetch(opts).await else {
		return Ok(Batch::Unanswered);
	};

	let pdus: Vec<Box<RawJsonValue>> = serde_json::from_slice(&outcome.bytes)?;
	let batch_size = pdus.len();
	let counted = self
		.backfill_pdus(room_id, &outcome.origin, pdus)
		.await;

	let prepended = counted.unwrap_or_else(identity);

	debug!(
		%room_id,
		%seed,
		origin = %outcome.origin,
		batch_size,
		prepended,
		deferred = counted.is_err(),
		"Processed backfill response",
	);

	let batch = match counted {
		| Err(_) => Batch::Deferred { prepended: prepended > 0 },
		| Ok(0) => Batch::Empty(outcome.origin.clone()),
		| Ok(_) => Batch::Prepended,
	};

	Ok(batch)
}

/// Ingests a backfill response, stopping at the first deferred event.
///
/// The events are taken in `ingestion_order`, so nothing older is prepended
/// past a deferred event. The count of prepended events comes back, as the
/// error when a deferred event stopped ingestion; each event's own ingestion
/// failure only leaves it out of the count.
#[implement(super::Service)]
#[tracing::instrument(skip(self, pdus), level = "debug")]
pub async fn backfill_pdus(
	&self,
	room_id: &RoomId,
	origin: &ServerName,
	pdus: Vec<Box<RawJsonValue>>,
) -> Result<usize, usize> {
	ingestion_order(pdus)
		.stream()
		.then(|pdu| {
			self.backfill_pdu(room_id, origin, pdu)
				.inspect_err(|e| debug_warn!(%room_id, %e, "Failed to add backfilled pdu"))
				.unwrap_or(Backfilled::Skipped)
		})
		.map(Ok)
		.ready_try_fold(0_usize, tally)
		.await
}

/// Orders a backfill response for ingestion: newest first by depth, and the
/// create event last whatever depth it claims.
///
/// An event's ancestors all have a smaller depth, so nothing ingested ahead of
/// a deferred event is among them, and the create event, whose prepend ends
/// backfill for the room, comes after every other event. An event whose type
/// or depth cannot be read goes first, for ingestion to reject.
fn ingestion_order(pdus: Vec<Box<RawJsonValue>>) -> impl Iterator<Item = Box<RawJsonValue>> {
	pdus.into_iter()
		.sorted_by_cached_key(|pdu| placement(pdu))
}

fn placement(pdu: &RawJsonValue) -> Option<(bool, Reverse<UInt>)> {
	serde_json::from_str(pdu.get())
		.ok()
		.map(|Placement { kind, depth }| (kind == TimelineEventType::RoomCreate, Reverse(depth)))
}

/// Counts a batch's prepended events, ending at the first deferred one.
///
/// The count so far comes back as the error, so nothing after a deferred event
/// is ingested.
fn tally(prepended: usize, outcome: Backfilled) -> Result<usize, usize> {
	match outcome {
		| Backfilled::Skipped => Ok(prepended),
		| Backfilled::Prepended => Ok(prepended.saturating_add(1)),
		| Backfilled::Deferred => Err(prepended),
	}
}

/// Finds the events, oldest first, below which history is still missing.
///
/// The scan rises from the room's first event in stream order and ends at the
/// create event. It passes each event whose frontier names only events already
/// in the timeline: the event backfill prepended last can be such a one, and
/// asking from it could then never fetch anything new. An unreadable event is
/// passed over.
#[implement(super::Service)]
pub fn backfill_edges<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = PduEvent> + Send + 'a {
	self.pdus(None, room_id, None)
		.ready_filter_map(move |item| {
			item.map(at!(1))
				.inspect_err(|error| debug_warn!(%room_id, %error, "Skipped an unreadable event"))
				.ok()
		})
		.ready_take_while(|pdu| *pdu.event_type() != TimelineEventType::RoomCreate)
		.filter_map(async |pdu| {
			self.misses_history(room_id, &pdu)
				.await
				.then_some(pdu)
		})
}

/// Walks the frontier from the room's first event, then from the scanned event.
///
/// The first event's seeds come first, so a scanned event only adds requests
/// behind them.
#[implement(super::Service)]
pub fn backfill_seeds<'a>(
	&'a self,
	room_id: &'a RoomId,
	first: &'a PduEvent,
	scanned: Option<&'a PduEvent>,
) -> impl Stream<Item = OwnedEventId> + Send + 'a {
	once(first)
		.chain(scanned)
		.stream()
		.flat_map(|root| self.backfill_frontier(room_id, root))
		.map(at!(0))
}

/// Whether the frontier from `pdu` names an event missing from the timeline.
///
/// A predecessor whose timeline id cannot be read counts as missing.
#[implement(super::Service)]
async fn misses_history(&self, room_id: &RoomId, pdu: &PduEvent) -> bool {
	self.backfill_frontier(room_id, pdu)
		.flat_map(|(_, unheld)| unheld.into_iter().stream())
		.any(async |prev| self.get_pdu_id(&prev).is_err().await)
		.await
}

/// Finds the events below which backfill still has history to ask for.
///
/// The walk starts at `oldest` and passes breadth first through each
/// predecessor stored as a soft-failed outlier of the room, which backfill
/// would withhold. It yields every event it reaches, `oldest` first, that names
/// a predecessor of any other kind, with those predecessors, since a request
/// from that event reaches them. An event met a second time is not walked
/// again.
#[implement(super::Service)]
pub fn backfill_frontier<'a>(
	&'a self,
	room_id: &'a RoomId,
	oldest: &'a PduEvent,
) -> impl Stream<Item = (OwnedEventId, Unheld)> + Send + 'a {
	let root = (oldest.event_id.clone(), oldest.prev_events.clone());
	let start = (Pending::from([root]), Passed::new());

	unfold(start, async |(pending, passed)| {
		let ((event_id, prev_events), pending) = pop(pending)?;
		let (pending, passed, unheld) = prev_events
			.iter()
			.stream()
			.then(async |prev| {
				let withheld = self
					.services
					.pdu_metadata
					.get_soft_failed_outlier(room_id, prev)
					.ok()
					.await;

				(prev, withheld)
			})
			.ready_fold((pending, passed, Unheld::new()), enqueue)
			.await;

		let reaches = unheld.is_empty().is_false();

		Some((reaches.then_some((event_id, unheld)), (pending, passed)))
	})
	.ready_filter_map(identity)
}

fn pop(mut pending: Pending) -> Option<((OwnedEventId, PrevEvents), Pending)> {
	let next = pending.pop_front()?;

	Some((next, pending))
}

fn enqueue(
	(mut pending, mut passed, mut unheld): (Pending, Passed, Unheld),
	(prev, withheld): (&OwnedEventId, Option<PduEvent>),
) -> (Pending, Passed, Unheld) {
	match withheld {
		| None => unheld.push(prev.clone()),
		| Some(PduEvent { event_id, prev_events, .. }) =>
			if passed.insert(event_id.clone()) {
				pending.push_back((event_id, prev_events));
			},
	}

	(pending, passed, unheld)
}

/// Whether a request can follow `batch`, so the walk is worth running.
///
/// It holds whenever `retry` can answer one: the first request leads on unless
/// no candidate answered it, and every later one is counted. A deferred event
/// ends the call.
fn walks(attempt: &Attempt, batch: &Batch) -> bool {
	match (&attempt.seed, batch) {
		| (_, Batch::Deferred { .. }) => false,
		| (Seed::Oldest, _) => !matches!(batch, Batch::Unanswered),
		| (Seed::Frontier(_) | Seed::Fallback, _) => attempt.remaining > 1,
	}
}

/// The request after one backfill request, while any are left.
///
/// `frontier` is the walk after `batch` from the oldest event and then from the
/// call's scanned event. An answered first request is followed by the first
/// other frontier event; after a prepend the oldest event is not asked again. A
/// seed that progressed is followed by the next frontier event not yet asked,
/// so a prepend does not end the call while a branch of a withheld fork is left
/// and the budget allows another request; one that made no progress hands the
/// call to the oldest event's remaining candidates first. Once the seeds are
/// spent, the oldest event is asked again of those candidates if it is itself
/// on the frontier. The request the walk first moves after is not counted, so a
/// walk resumed over stored withheld events keeps the whole budget. A deferred
/// event ends the call, so no other origin or seed is asked past it.
fn retry(
	attempt: Attempt,
	batch: &Batch,
	oldest: &EventId,
	frontier: &[OwnedEventId],
) -> Option<Attempt> {
	if matches!(batch, Batch::Deferred { .. }) {
		return None;
	}

	let Attempt { seed, anchor, asked, remaining } = attempt;
	let charged = remaining.saturating_sub(1);
	let anchor: Candidates = match (&seed, batch) {
		| (Seed::Frontier(_), _) => anchor,
		| (_, Batch::Empty(origin)) => anchor
			.into_iter()
			.filter(|server| server != origin)
			.collect(),

		// The oldest event was answered, or none of its candidates answered.
		| _ => Candidates::new(),
	};

	let stuck = match (&seed, batch) {
		| (Seed::Frontier(seed), Batch::Empty(_)) => frontier.contains(seed),
		| (Seed::Frontier(_), Batch::Unanswered) => true,
		| _ => false,
	};

	let next = frontier
		.iter()
		.find(|event_id| *event_id != oldest && !asked.contains(*event_id))
		.cloned();

	let reask = !anchor.is_empty();
	let oldest_open = reask && frontier.iter().any(|event_id| event_id == oldest);
	let (seed, remaining) = match (seed, batch, next) {
		| (Seed::Oldest, Batch::Empty(_) | Batch::Prepended, Some(next)) =>
			(Seed::Frontier(next), remaining),
		| (Seed::Oldest, Batch::Empty(_), None) => (Seed::Oldest, charged),
		| (Seed::Oldest, ..) => return None,
		| (Seed::Frontier(_), ..) if stuck && reask => (Seed::Fallback, charged),
		| (Seed::Fallback, Batch::Empty(_), _) if reask => (Seed::Fallback, charged),
		| (_, _, Some(next)) => (Seed::Frontier(next), charged),
		| (Seed::Frontier(_), ..) if oldest_open => (Seed::Fallback, charged),
		| _ => return None,
	};

	let asked = match &seed {
		| Seed::Oldest | Seed::Fallback => asked,
		| Seed::Frontier(seed) => ask(asked, seed.clone()),
	};

	// A frontier seed is asked of every candidate, and there is at least one.
	let due = remaining > 0 && (matches!(seed, Seed::Frontier(_)) || reask);

	due.then_some(Attempt { seed, anchor, asked, remaining })
}

fn ask(mut asked: Passed, seed: OwnedEventId) -> Passed {
	asked.insert(seed);
	asked
}

#[implement(super::Service)]
/// Finds the nearest event to a timestamp with a federation fallback.
///
/// The local answer is retained unless a closer remote claim can be ingested.
/// The ingested event must belong to this room and its actual timestamp must
/// remain on the requested side of the query.
pub async fn get_event_id_near_ts_with_fallback(
	&self,
	room_id: &RoomId,
	ts: MilliSecondsSinceUnixEpoch,
	dir: Direction,
) -> Result<(MilliSecondsSinceUnixEpoch, OwnedEventId)> {
	let local = self.get_event_id_near_ts(room_id, ts, dir).await;

	let federate = match &local {
		| Err(_) => true,
		| Ok((_, event_id)) =>
			dir == Direction::Forward && self.is_start_edge_hit(room_id, event_id).await,
	};

	if !federate {
		return local;
	}

	let candidates = self.backfill_candidates(room_id).await;

	if candidates.is_empty() {
		return local;
	}

	let opts = Opts::new(Op::TimestampToEvent, room_id.to_owned())
		.ts(ts)
		.dir(dir)
		.candidates(candidates)
		.checks(false);

	let Ok(outcome) = self.services.fetcher.fetch(opts).await else {
		return local;
	};

	let Ok(TimestampHit { event_id, origin_server_ts }) = serde_json::from_slice(&outcome.bytes)
	else {
		return local;
	};

	if let Ok((local_ts, local_id)) = &local
		&& !nearer(dir, origin_server_ts, *local_ts)
	{
		return Ok((*local_ts, local_id.clone()));
	}

	// Fail closed: an un-ingested event can't be visibility-checked, so keep local.
	let Ok(pdu) = self
		.backfill_event(room_id, &event_id, &outcome.origin)
		.inspect_err(|e| debug_warn!(%room_id, error = ?e, "timestamp fallback backfill failed"))
		.await
	else {
		return local;
	};

	let actual_ts = pdu.origin_server_ts();
	let matches_direction = match dir {
		| Direction::Forward => actual_ts >= ts,
		| Direction::Backward => actual_ts <= ts,
	};

	if actual_ts != origin_server_ts || !matches_direction {
		debug_warn!(
			%room_id,
			%event_id,
			?dir,
			?ts,
			?origin_server_ts,
			?actual_ts,
			"timestamp fallback claim was inconsistent with the ingested event"
		);
		return local;
	}

	Ok((actual_ts, event_id))
}

#[implement(super::Service)]
async fn is_start_edge_hit(&self, room_id: &RoomId, event_id: &EventId) -> bool {
	self.first_item_in_room(room_id)
		.await
		.is_ok_and(|(_, first)| {
			*first.event_type() != TimelineEventType::RoomCreate && first.event_id() == event_id
		})
}

fn nearer(dir: Direction, a: MilliSecondsSinceUnixEpoch, b: MilliSecondsSinceUnixEpoch) -> bool {
	match dir {
		| Direction::Forward => a < b,
		| Direction::Backward => a > b,
	}
}

#[implement(super::Service)]
async fn backfill_event(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	origin: &ServerName,
) -> Result<PduEvent> {
	let opts = Opts::new(Op::Backfill, room_id.to_owned())
		.event_id(event_id.to_owned())
		.candidates([origin.to_owned()])
		.backfill_limit(BACKFILL_LIMIT);

	let outcome = self.services.fetcher.fetch(opts).await?;

	let pdus: Vec<Box<RawJsonValue>> = serde_json::from_slice(&outcome.bytes)?;

	// The target counts only once prepended, whatever else the batch held.
	self.backfill_pdus(room_id, &outcome.origin, pdus)
		.await
		.ok();

	self.get_pdu_count(event_id).await?;

	let pdu = self.get_pdu(event_id).await?;

	if pdu.room_id() != room_id {
		return Err!(Request(NotFound("Timestamp fallback target belongs to another room.")));
	}

	Ok(pdu)
}

/// Fetches one remote event and persists it through the backfill path.
///
/// Fetcher checks are disabled only for retrieval; `backfill_pdu`
/// performs signature, hash, and authorization validation. An event already
/// accepted, a soft-failed one, or one the room's policy server does not
/// recommend completes successfully without insertion, while any other stored
/// outlier is still validated and promoted.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn fetch_remote_event(&self, room_id: &RoomId, event_id: &EventId) -> Result {
	let opts = Opts::new(Op::Event, room_id.to_owned())
		.event_id(event_id.to_owned())
		.checks(false);

	let outcome = self.services.fetcher.fetch(opts).await?;

	let pdu: Box<RawJsonValue> = serde_json::from_slice(&outcome.bytes)?;

	self.backfill_pdu(room_id, &outcome.origin, pdu)
		.await?;

	Ok(())
}

/// Validates and inserts one remotely supplied event as backfilled history.
///
/// An already accepted duplicate is skipped without insertion, as is a
/// soft-failed event, which stays the outlier it was stored as, and one the
/// room's policy server refuses, which is marked soft-failed. One it gives no
/// verdict on is deferred. Any other new event or stored outlier receives a
/// negative backfill count, moves to accepted storage, updates its timestamp
/// and ID indexes, and adds searchable message or topic content.
#[implement(super::Service)]
#[tracing::instrument(skip(self, pdu), level = "debug")]
pub async fn backfill_pdu(
	&self,
	room_id: &RoomId,
	origin: &ServerName,
	pdu: Box<RawJsonValue>,
) -> Result<Backfilled> {
	let (_, event_id, value) = self
		.services
		.event_handler
		.parse_incoming_pdu(&pdu)
		.await?;

	// Lock so we cannot backfill the same pdu twice at the same time; only after
	// parsing, which can wait on this lock.
	let mutex_lock = self
		.services
		.event_handler
		.mutex_federation
		.lock(room_id)
		.await;

	let soft_failed = self
		.services
		.pdu_metadata
		.get_soft_failed_outlier(room_id, &event_id)
		.await
		.present()?
		.is_some();

	// Ahead of ingestion, so the remote copy never replaces the stored outlier; a
	// failed read errs rather than letting the copy in.
	if soft_failed {
		debug!(%event_id, "Soft-failed event stays out of backfilled history");
		return Ok(Backfilled::Skipped);
	}

	let existed = self
		.services
		.event_handler
		.handle_incoming_pdu(origin, room_id, &event_id, value, false)
		.await?
		.map(at!(1))
		.is_some_and(is_false!());

	// Bail if the PDU already exists; a duplicate insertion is not good.
	if existed {
		return Ok(Backfilled::Skipped);
	}

	let pdu = self.get_pdu(&event_id);

	let value = self.get_pdu_json(&event_id);

	let shortroomid = self.services.short.get_shortroomid(room_id);

	let (pdu, value, shortroomid) = try_join3(pdu, value, shortroomid).await?;

	// Asked before the insert lock, so no `/sign` round trip ever holds it.
	let (check, value) = self.policy_check(&pdu, value).await;

	if let Some(withheld) = self.withhold(&pdu, check).await {
		return Ok(withheld);
	}

	let insert_lock = self.mutex_insert.lock(room_id).await;

	// A pdu_id is not returned from handle_incoming_pdu() when accepting a new
	// event on this codepath. The pdu_id is instead created here in ℤ−
	let count = self.services.globals.next_count();
	let count: i64 = (*count).try_into()?;
	let pdu_id: RawPduId = PduId {
		shortroomid,
		count: PduCount::Backfilled(validated!(0 - count)),
	}
	.into();

	// Insert pdu
	self.prepend_backfill_pdu(
		&pdu_id,
		room_id,
		&event_id,
		u64::from(pdu.origin_server_ts),
		&value,
	);
	drop(insert_lock);

	match pdu.kind {
		| TimelineEventType::RoomMessage => {
			if let Ok(ExtractBody { body: Some(body) }) = pdu.get_content() {
				self.services
					.search
					.index_pdu(shortroomid, &pdu_id, &body);
			}
		},
		| TimelineEventType::RoomTopic =>
			if let Some(topic) = pdu.get_content().ok().and_then(plain_text_topic) {
				self.services
					.search
					.index_pdu(shortroomid, &pdu_id, &topic);
			},
		| _ => {},
	}

	drop(mutex_lock);

	debug!("Prepended backfill pdu");
	Ok(Backfilled::Prepended)
}

#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "debug")]
async fn policy_check(
	&self,
	pdu: &PduEvent,
	mut json: CanonicalJsonObject,
) -> (PolicyCheck, CanonicalJsonObject) {
	let check = self
		.services
		.event_handler
		.verify_or_fetch_inbound_policy_signature(&mut json, pdu)
		.await;

	(check, json)
}

/// Withholds a backfilled event its room's policy server does not recommend.
///
/// A refused event is marked soft-failed, as the live path marks one, so later
/// offers skip it. One without a verdict stays an unmarked outlier and is asked
/// about again when backfill offers it next.
#[implement(super::Service)]
async fn withhold(&self, pdu: &PduEvent, check: PolicyCheck) -> Option<Backfilled> {
	let (event_id, room_id) = (pdu.event_id(), pdu.room_id());

	match check {
		| PolicyCheck::NotApplicable | PolicyCheck::Pass | PolicyCheck::Missing => None,
		| PolicyCheck::Invalid => {
			self.mark_refused(room_id, event_id).await;
			debug_warn!(%event_id, %room_id, "Policy server refused a backfilled event");
			Some(Backfilled::Skipped)
		},
		| PolicyCheck::NoVerdict => {
			debug_warn!(%event_id, %room_id, "No policy server verdict; deferring backfill");
			Some(Backfilled::Deferred)
		},
	}
}

/// Marks a refused event soft-failed while its room still exists.
///
/// The room's state lock orders the write against a room deletion, whose sweep
/// clears the room's markers, as it orders the live path's marker; a room
/// deleted while the policy server was asked gets no marker.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
async fn mark_refused(&self, room_id: &RoomId, event_id: &EventId) {
	let _state_lock = self.services.state.mutex.lock(room_id).await;

	let exists = self
		.services
		.short
		.get_shortroomid(room_id)
		.is_ok()
		.await;

	if exists {
		self.services
			.pdu_metadata
			.mark_event_soft_failed(event_id);
	}
}

#[implement(super::Service)]
fn prepend_backfill_pdu(
	&self,
	pdu_id: &RawPduId,
	room_id: &RoomId,
	event_id: &EventId,
	origin_server_ts: u64,
	json: &CanonicalJsonObject,
) {
	let mut txn = self.db.db.txn();

	txn.raw_put(&self.db.pduid_pdu, pdu_id, Json(json));
	txn.insert_raw(&self.db.eventid_pduid, event_id, pdu_id);
	txn.del_raw(&self.db.eventid_outlierpdu, event_id);

	let count_key = bias_count(pdu_id.count());
	let key = (room_id, origin_server_ts, count_key);
	txn.put_raw(&self.db.roomid_tscount_pducount, key, pdu_id.count());

	txn.execute();
}
