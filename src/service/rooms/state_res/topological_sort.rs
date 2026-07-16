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
	collections::{BinaryHeap, HashMap, HashSet},
};

use futures::{Stream, TryFutureExt, TryStreamExt, stream::try_unfold};
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
/// Returns the ordered list of event IDs from earliest to latest. Every event
/// in the graph appears exactly once; a reference to an event absent from the
/// graph is treated as a non-edge rather than dropping the referencing event.
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
	graph: HashMap<OwnedEventId, ReferencedIds>,
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

	// A reference absent from the graph is unresolvable and not an out-edge.
	let horizon = graph
		.iter()
		.filter(|(_, references)| {
			!references
				.iter()
				.any(|reference| graph.contains_key(reference))
		})
		.try_stream()
		.and_then(async |(event_id, _)| Ok(Reverse(query(event_id.clone()).await?)))
		.try_collect::<BinaryHeap<Reverse<TieBreaker>>>()
		.await?;

	kahn_sort(horizon, graph, &incoming, &query)
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
	try_unfold((heap, graph), async move |(mut heap, graph)| {
		let Some(Reverse(item)) = heap.pop() else {
			return Ok(None);
		};

		let references = incoming.get(&item.event_id).cloned();
		let state = (item.event_id, (heap, graph));
		references
			.into_iter()
			.flatten()
			.try_stream()
			.try_fold(state, async |(event_id, (mut heap, mut graph)), parent_id| {
				graph
					.get_mut(&parent_id)
					.expect("contains all parent_ids")
					.retain(is_not_equal_to!(&event_id));

				// References to absent events never resolve; gate on present out-edges only.
				if !graph[&parent_id]
					.iter()
					.any(|reference| graph.contains_key(reference))
				{
					heap.push(Reverse(query(parent_id.clone()).await?));
				}

				Ok::<_, Error>((event_id, (heap, graph)))
			})
			.map_ok(Some)
			.await
	})
}

/// Tests whether the events in `order` are in reverse topological power
/// ordering with respect to `graph`: each event appears after every event it
/// references that is present in `graph`.
///
/// A reference absent from `graph` is a non-edge, as in [`topological_sort`].
/// The check covers relative order only: it certifies that the events present
/// in `order` are mutually consistent, not that `order` is complete or
/// duplicate-free, and (lacking the tie-breaker) not that it equals the exact
/// sequence [`topological_sort`] would select.
#[expect(clippy::implicit_hasher)]
pub fn is_topologically_sorted<'a, Order>(
	order: Order,
	graph: &HashMap<OwnedEventId, ReferencedIds>,
) -> bool
where
	Order: IntoIterator<Item = &'a OwnedEventId>,
{
	order
		.into_iter()
		.try_fold(HashSet::with_capacity(graph.len()), |mut seen, event_id| {
			let satisfied = graph
				.get(event_id)
				.into_iter()
				.flatten()
				.filter(|reference| graph.contains_key(*reference))
				.all(|reference| seen.contains(reference));

			seen.insert(event_id);
			satisfied.then_some(seen)
		})
		.is_some()
}

/// Whether `items` are already in reverse topological order, reading each
/// item's references in place instead of from a prebuilt graph. An item must
/// follow every item it references that is also present among `items`; a
/// reference to an absent item is a non-edge. `id` reads an item's identifier,
/// `references` its outgoing references.
///
/// This allocates nothing, scanning the remaining items for each reference
/// rather than building a seen-set. The scan is quadratic, so it suits short
/// sequences; [`is_topologically_sorted`] is the better pick for a large
/// sequence or one whose graph is already built.
pub fn is_topologically_sorted_in_place<'a, T, Id, Refs, Ref>(
	items: &'a [T],
	id: Id,
	references: Refs,
) -> bool
where
	Id: Fn(&'a T) -> &'a str,
	Refs: Fn(&'a T) -> Ref,
	Ref: Iterator<Item = &'a str>,
{
	if items.len() < 2 {
		return true;
	}

	items.iter().enumerate().all(|(i, item)| {
		references(item).all(|reference| {
			items[i..]
				.iter()
				.all(|other| id(other) != reference)
		})
	})
}

#[cfg(test)]
mod tests {
	use super::is_topologically_sorted_in_place;

	fn sorted(items: &[(&str, &[&str])]) -> bool {
		is_topologically_sorted_in_place(
			items,
			|item: &(&str, &[&str])| item.0,
			|item: &(&str, &[&str])| item.1.iter().copied(),
		)
	}

	#[test]
	fn empty_or_single_is_sorted() {
		assert!(sorted(&[]));
		assert!(sorted(&[("a", &[])]));
	}

	#[test]
	fn parents_before_children() {
		assert!(sorted(&[("a", &[]), ("b", &["a"]), ("c", &["a", "b"])]));
	}

	#[test]
	fn child_before_parent_is_unsorted() {
		assert!(!sorted(&[("b", &["a"]), ("a", &[])]));
	}

	#[test]
	fn absent_reference_is_a_non_edge() {
		assert!(sorted(&[("b", &["x"]), ("a", &["x"])]));
	}

	#[test]
	fn self_reference_is_unsorted() {
		assert!(!sorted(&[("a", &["a"]), ("b", &[])]));
	}

	#[test]
	fn later_duplicate_of_a_parent_is_unsorted() {
		assert!(!sorted(&[("a", &[]), ("b", &["a"]), ("a", &[])]));
	}
}
