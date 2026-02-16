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

use futures::TryStreamExt;
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedEventId, events::room::power_levels::UserPowerLevel,
};
use tuwunel_core::{
	Error, Result, is_not_equal_to, smallvec::SmallVec, utils::stream::IterStream, validated,
};

pub type ReferencedIds = SmallVec<[OwnedEventId; 3]>;
type PduInfo = (UserPowerLevel, MilliSecondsSinceUnixEpoch);

#[derive(PartialEq, Eq)]
struct TieBreaker {
	power_level: UserPowerLevel,
	origin_server_ts: MilliSecondsSinceUnixEpoch,
	event_id: OwnedEventId,
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
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		graph = graph.len(),
	)
)]
#[expect(clippy::implicit_hasher, clippy::or_fun_call)]
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
		Ok::<_, Error>(TieBreaker { power_level, origin_server_ts, event_id })
	};

	let max_edges = graph
		.values()
		.map(ReferencedIds::len)
		.fold(graph.len(), |a, c| validated!(a + c));

	// We consider that the DAG is directed from most recent events to oldest
	// events, so an event is an incoming edge to its referenced events.
	// zero_outdegs: Vec of events that have an outdegree of zero (no outgoing
	// edges), i.e. the oldest events. incoming_edges_map: Map of event to the list
	// of events that reference it in its referenced events.
	let init = (
		Vec::with_capacity(max_edges),
		HashMap::<OwnedEventId, ReferencedIds>::with_capacity(max_edges),
	);

	// Populate the list of events with an outdegree of zero, and the map of
	// incoming edges.
	let (zero_outdeg, incoming_edges) = graph
		.iter()
		.try_stream()
		.try_fold(
			init,
			async |(mut zero_outdeg, mut incoming_edges), (event_id, outgoing_edges)| {
				if outgoing_edges.is_empty() {
					// `Reverse` because `BinaryHeap` sorts largest -> smallest and we need
					// smallest -> largest.
					zero_outdeg.push(Reverse(query(event_id.clone()).await?));
				}

				for referenced_event_id in outgoing_edges {
					let references = incoming_edges
						.entry(referenced_event_id.into())
						.or_default();

					if !references.contains(event_id) {
						references.push(event_id.into());
					}
				}

				Ok((zero_outdeg, incoming_edges))
			},
		)
		.await?;

	// Map of event to the list of events in its referenced events.
	let mut outgoing_edges_map = graph.clone();

	// Use a BinaryHeap to keep the events with an outdegree of zero sorted.
	let mut heap = BinaryHeap::from(zero_outdeg);
	let mut sorted = Vec::with_capacity(max_edges);

	// Apply Kahn's algorithm.
	// https://en.wikipedia.org/wiki/Topological_sorting#Kahn's_algorithm
	while let Some(Reverse(item)) = heap.pop() {
		for parent_id in incoming_edges
			.get(&item.event_id)
			.unwrap_or(&ReferencedIds::new())
		{
			let outgoing_edges = outgoing_edges_map
				.get_mut(parent_id)
				.expect("outgoing_edges should contain all event_ids");

			outgoing_edges.retain(is_not_equal_to!(&item.event_id));
			if !outgoing_edges.is_empty() {
				continue;
			}

			// Push on the heap once all the outgoing edges have been removed.
			heap.push(Reverse(query(parent_id.clone()).await?));
		}

		sorted.push(item.event_id);
	}

	Ok(sorted)
}
