//! Sorts the given event graph using reverse topological power ordering.
//!
//! Definition in the specification:
//!
//! The reverse topological power ordering of a set of events is the
//! lexicographically smallest topological ordering based on the DAG formed by
//! referenced events (prev or auth, determined by caller). The reverse
//! topological power ordering is ordered from earliest event to latest. For
//! comparing two equal topological orderings to determine which is the
//! lexicographically smallest, the following comparison relation on events is
//! used: for events x and y, x < y if
//!
//! 1. x’s sender has greater power level than y’s sender, when looking at their
//!    respective referenced events; or
//! 2. the senders have the same power level, but x’s origin_server_ts is less
//!    than y’s origin_server_ts; or
//! 3. the senders have the same power level and the events have the same
//!    origin_server_ts, but x’s event_id is less than y’s event_id.
//!
//! The reverse topological power ordering can be found by sorting the events
//! using Kahn’s algorithm for topological sorting, and at each step selecting,
//! among all the candidate vertices, the smallest vertex using the above
//! comparison relation.

use std::{
	cmp::{Ordering, Reverse},
	collections::{BinaryHeap, HashMap},
};

use futures::{Stream, TryFutureExt, TryStreamExt, stream::try_unfold};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue as JsonValue, MilliSecondsSinceUnixEpoch,
	OwnedEventId, OwnedRoomId, events::room::power_levels::UserPowerLevel, int, uint,
};
use tuwunel_core::{
	Error, Result, err, is_equal_to, is_not_equal_to, matrix::Event, ref_at, smallvec::SmallVec,
	utils::stream::IterStream, validated,
};

pub type ReferencedIds = SmallVec<[OwnedEventId; 3]>;
type PduInfo = (UserPowerLevel, MilliSecondsSinceUnixEpoch);
type RawPdu = (usize, (OwnedRoomId, OwnedEventId, CanonicalJsonObject));

#[derive(PartialEq, Eq)]
struct TieBreaker {
	event_id: OwnedEventId,
	power_level: UserPowerLevel,
	origin_server_ts: MilliSecondsSinceUnixEpoch,
}

// NOTE: the power level comparison is "backwards" intentionally.
impl Ord for TieBreaker {
	fn cmp(&self, other: &Self) -> Ordering {
		other
			.power_level
			.cmp(&self.power_level)
			.then(self.origin_server_ts.cmp(&other.origin_server_ts))
			.then(self.event_id.cmp(&other.event_id))
	}
}

impl PartialOrd for TieBreaker {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

/// Reverse topological sort of raw wire PDU json's. Note that PL's are not
/// considered for tiebreaking here; this should never be used for an
/// authoritative order.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn topological_sort_pdus(mut pdus: Vec<RawPdu>) -> Result<Vec<RawPdu>> {
	let origin_server_ts = |json: &CanonicalJsonObject| {
		json.get("origin_server_ts")
			.and_then(JsonValue::as_integer)
			.map(usize::try_from)
			.and_then(Result::ok)
			.map(TryInto::<ruma::UInt>::try_into)
			.and_then(Result::ok)
			.map(MilliSecondsSinceUnixEpoch)
	};

	let prev_events = |json: &CanonicalJsonObject| {
		json.get("prev_events")
			.and_then(JsonValue::as_array)
			.into_iter()
			.flat_map(|array| array.iter())
			.filter_map(JsonValue::as_str)
			.map(TryInto::try_into)
			.map(|res| res.map_err(Into::into))
			.collect::<Result<ReferencedIds>>()
	};

	let ord_by = async |event_id: OwnedEventId| {
		let origin_server_ts = pdus
			.iter()
			.map(ref_at!(1))
			.find(|(_, other, _)| event_id.eq(other))
			.and_then(|(_, _, json)| origin_server_ts(json))
			.ok_or_else(|| err!(Request(NotFound("Error sorting PDU's"))))?;

		Ok((int!(0).into(), origin_server_ts))
	};

	let prev_ids = pdus
		.iter()
		.map(ref_at!(1))
		.map(ref_at!(2))
		.map(prev_events);

	let graph = pdus
		.iter()
		.map(ref_at!(1))
		.map(ref_at!(1))
		.cloned()
		.zip(prev_ids)
		.map(|(id, prev)| prev.map(|prev| (id, prev)))
		.collect::<Result<HashMap<OwnedEventId, ReferencedIds>>>()?;

	let sorted = topological_sort(&graph, &ord_by).await?;

	pdus.sort_by_key(|(_, (_, event_id, _))| {
		sorted
			.iter()
			.position(is_equal_to!(event_id))
			.expect("all pdu event_id's present in sorted output")
	});

	Ok(pdus)
}

/// Reverse topological sort of impl Event's. Note that PL's are not considered
/// for tiebreaking here; this should never be used for an authoritative order.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn topological_sort_events<Pdu: Event>(mut pdus: Vec<Pdu>) -> Vec<Pdu> {
	let ord_by = async |event_id: OwnedEventId| {
		let origin_server_ts = pdus
			.iter()
			.find(|&pdu| event_id.eq(pdu.event_id()))
			.map(Event::origin_server_ts)
			.unwrap_or_else(|| MilliSecondsSinceUnixEpoch(uint!(0)));

		Ok((int!(0).into(), origin_server_ts))
	};

	let event_ids = pdus
		.iter()
		.map(Event::event_id)
		.map(ToOwned::to_owned);

	let prev_ids = pdus
		.iter()
		.map(Event::prev_events)
		.map(|prev_events| prev_events.map(ToOwned::to_owned).collect());

	let graph = event_ids.zip(prev_ids).collect();
	let sorted = topological_sort(&graph, &ord_by)
		.await
		.expect("query callback never errors; errors never propagate");

	pdus.sort_by_key(|pdu| {
		sorted
			.iter()
			.position(is_equal_to!(pdu.event_id()))
			.expect("all pdu event_id's present in sorted output")
	});

	pdus
}

/// Sorts the given event graph using reverse topological power ordering.
///
/// ## Arguments
///
/// * `graph` - The graph to sort. A map of event ID to its referenced events
///   that are in the full conflicted set.
///
/// * `query` - Function to obtain a (power level, origin_server_ts) of an event
///   for breaking ties.
///
/// ## Returns
///
/// Returns the ordered list of event IDs from earliest to latest.
///
/// We consider that the DAG is directed from most recent events to oldest
/// events, so an event is an incoming edge to its referenced events.
/// zero_outdegs: Vec of events that have an outdegree of zero (no outgoing
/// edges), i.e. the oldest events. incoming_edges_map: Map of event to the list
/// of events that reference it in its referenced events.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		graph = graph.len(),
	)
)]
#[expect(clippy::implicit_hasher)]
pub async fn topological_sort<Query, Fut>(
	graph: &HashMap<OwnedEventId, ReferencedIds>,
	query: &Query,
) -> Result<Vec<OwnedEventId>>
where
	Query: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<PduInfo>> + Send,
{
	let query = async |event_id: OwnedEventId| {
		let (power_level, origin_server_ts) = query(event_id.clone()).await?;
		Ok::<_, Error>(TieBreaker { event_id, power_level, origin_server_ts })
	};

	let max_edges = graph
		.values()
		.map(ReferencedIds::len)
		.fold(graph.len(), |a, c| validated!(a + c));

	let incoming = graph
		.iter()
		.flat_map(|(event_id, out)| {
			out.iter()
				.map(move |reference| (event_id, reference))
		})
		.fold(HashMap::with_capacity(max_edges), |mut incoming, (event_id, reference)| {
			let references: &mut ReferencedIds = incoming.entry(reference.clone()).or_default();

			if !references.contains(event_id) {
				references.push(event_id.clone());
			}

			incoming
		});

	let horizon = graph
		.iter()
		.filter(|(_, references)| references.is_empty())
		.try_stream()
		.and_then(async |(event_id, _)| Ok(Reverse(query(event_id.clone()).await?)))
		.try_collect::<BinaryHeap<Reverse<TieBreaker>>>()
		.await?;

	kahn_sort(horizon, graph.clone(), &incoming, &query)
		.try_collect()
		.await
}

// Apply Kahn's algorithm.
// https://en.wikipedia.org/wiki/Topological_sorting#Kahn's_algorithm
// Use a BinaryHeap to keep the events with an outdegree of zero sorted.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		heap = %heap.len(),
		graph = %graph.len(),
	)
)]
fn kahn_sort<Query, Fut>(
	heap: BinaryHeap<Reverse<TieBreaker>>,
	graph: HashMap<OwnedEventId, ReferencedIds>,
	incoming: &HashMap<OwnedEventId, ReferencedIds>,
	query: &Query,
) -> impl Stream<Item = Result<OwnedEventId>> + Send
where
	Query: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<TieBreaker>> + Send,
{
	try_unfold((heap, graph), move |(mut heap, graph)| async move {
		let Some(Reverse(item)) = heap.pop() else {
			return Ok(None);
		};

		let references = incoming.get(&item.event_id).cloned();
		let state = (item.event_id, (heap, graph));
		references
			.into_iter()
			.flatten()
			.try_stream()
			.try_fold(state, |(event_id, (mut heap, mut graph)), parent_id| async move {
				let out = graph
					.get_mut(&parent_id)
					.expect("contains all parent_ids");

				out.retain(is_not_equal_to!(&event_id));

				// Push on the heap once all the outgoing edges have been removed.
				if out.is_empty() {
					heap.push(Reverse(query(parent_id.clone()).await?));
				}

				Ok::<_, Error>((event_id, (heap, graph)))
			})
			.map_ok(Some)
			.await
	})
}
