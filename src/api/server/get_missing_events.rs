use std::collections::{HashMap, HashSet, VecDeque};

use axum::extract::State;
use ruma::{
	OwnedEventId, UInt, api::federation::event::get_missing_events,
	canonical_json::redact_in_place, events::TimelineEventType,
};
use tuwunel_core::{Result, debug, err};

use super::AccessCheck;
use crate::Ruma;

/// arbitrary number but synapse's is 20 and we can handle lots of these anyways
const LIMIT_MAX: usize = 50;
/// spec says default is 10
const LIMIT_DEFAULT: usize = 10;
/// bound the backward walk so a single request cannot fan out across
/// arbitrarily deep or wide reachable history.
const WALK_LIMIT_MAX: usize = 250;
/// # `POST /_matrix/federation/v1/get_missing_events/{roomId}`
///
/// Retrieves events that the sender is missing.
pub(crate) async fn get_missing_events_route(
	State(services): State<crate::State>,
	body: Ruma<get_missing_events::v1::Request>,
) -> Result<get_missing_events::v1::Response> {
	AccessCheck {
		services: &services,
		origin: body.origin(),
		room_id: &body.room_id,
		event_id: None,
	}
	.check()
	.await?;

	let limit = body
		.limit
		.try_into()
		.unwrap_or(LIMIT_DEFAULT)
		.min(LIMIT_MAX);

	let room_version = services
		.state
		.get_room_version(&body.room_id)
		.await?;
	let room_version_rules = services
		.state
		.get_room_version_rules(&body.room_id)
		.await?;

	let earliest_events: HashSet<OwnedEventId> = body.earliest_events.iter().cloned().collect();
	let mut queue: VecDeque<OwnedEventId> = body.latest_events.iter().cloned().collect();
	// `seen` only dedups the walk; it is not a proof any given id is a real,
	// locally-known boundary, so it must not be handed to `topo_sort_events` as
	// such (see `resolved` below).
	let mut seen: HashSet<OwnedEventId> = earliest_events.clone();
	// The set of ids `topo_sort_events` may treat as legitimate, already-known
	// boundaries: the request's own `earliest_events`, plus every event we
	// actually confirmed exists locally (whether or not it ended up in
	// `results`, e.g. it was below `min_depth` or was itself a latest_event).
	// Crucially this excludes ids that only ever sat in `seen` because the walk
	// limit cut the traversal short or because `get_pdu` failed -- those are
	// unresolved, not boundaries, so a result referencing one of them as a prev
	// must still be invalidated rather than silently accepted.
	let mut resolved: HashSet<OwnedEventId> = earliest_events.clone();
	let mut results: Vec<(OwnedEventId, Vec<OwnedEventId>, UInt, _)> = Vec::new();
	let mut walked = 0_usize;

	while let Some(event_id) = queue.pop_front() {
		if !seen.insert(event_id.clone()) {
			continue;
		}

		walked = walked.saturating_add(1);
		if walked > WALK_LIMIT_MAX {
			debug!(
				?body.origin,
				%event_id,
				walked,
				limit = WALK_LIMIT_MAX,
				"Reached get_missing_events walk limit"
			);
			break;
		}

		let Ok(pdu) = services.timeline.get_pdu(&event_id).await else {
			debug!(?body.origin, %event_id, "Event does not exist locally, skipping");
			continue;
		};
		resolved.insert(event_id.clone());

		if pdu.depth > body.min_depth {
			queue.extend(pdu.prev_events.iter().cloned());
		}

		if body.latest_events.contains(&event_id) {
			continue;
		}

		if pdu.depth < body.min_depth {
			continue;
		}

		if pdu.kind == TimelineEventType::RoomGuestAccess {
			continue;
		}

		let visible = services
			.state_accessor
			.server_can_see_event(body.origin(), &body.room_id, &event_id)
			.await;
		let mut event = services.timeline.get_pdu_json(&event_id).await?;

		if !visible {
			debug!(
				?body.origin,
				%event_id,
				room_id = %body.room_id,
				"Server cannot see event, redacting before returning and continuing traversal"
			);
			redact_in_place(&mut event, &room_version_rules.redaction, None).map_err(|e| {
				err!(Request(BadJson("Failed to redact event for federation response: {e}")))
			})?;
		}

		let event = services
			.state_accessor
			.erased_for_server(body.origin(), event)
			.await;

		let event = services
			.federation
			.format_pdu_into(event, Some(&room_version))
			.await;

		results.push((event_id, pdu.prev_events.into_vec(), pdu.depth, event));
	}

	let sorted_ids = topo_sort_events(
		results
			.iter()
			.map(|(event_id, prev_events, depth, _)| {
				(event_id.clone(), prev_events.clone(), *depth)
			}),
		&resolved,
		body.min_depth,
	);

	let mut event_map: HashMap<OwnedEventId, _> = results
		.into_iter()
		.map(|(event_id, _, _, event)| (event_id, event))
		.collect();

	let events = sorted_ids
		.into_iter()
		.filter_map(|event_id| event_map.remove(&event_id))
		.take(limit)
		.collect();

	Ok(get_missing_events::v1::Response { events })
}

fn topo_sort_events(
	events: impl IntoIterator<Item = (OwnedEventId, Vec<OwnedEventId>, UInt)>,
	reached_events: &HashSet<OwnedEventId>,
	min_depth: UInt,
) -> Vec<OwnedEventId> {
	let events: Vec<_> = events.into_iter().collect();
	let mut in_degree: HashMap<OwnedEventId, usize> = HashMap::with_capacity(events.len());
	let mut graph: HashMap<OwnedEventId, Vec<OwnedEventId>> =
		HashMap::with_capacity(events.len());
	let mut depth_map: HashMap<OwnedEventId, UInt> = HashMap::with_capacity(events.len());

	for (event_id, _, depth) in &events {
		in_degree.entry(event_id.clone()).or_insert(0);
		depth_map.insert(event_id.clone(), *depth);
	}

	let mut invalid = HashSet::new();

	for (event_id, prev_events, depth) in &events {
		for prev_event in prev_events {
			if in_degree.contains_key(prev_event) {
				graph
					.entry(prev_event.clone())
					.or_default()
					.push(event_id.clone());
				let degree = in_degree
					.get_mut(event_id)
					.expect("event must be present in in_degree");
				*degree = degree.checked_add(1).expect("in-degree overflow");
			} else if !reached_events.contains(prev_event) && *depth > min_depth {
				invalid.insert(event_id.clone());
			}
		}
	}

	let mut queue: VecDeque<_> = invalid.iter().cloned().collect();
	while let Some(inv) = queue.pop_front() {
		if let Some(children) = graph.get(&inv) {
			for child in children {
				if invalid.insert(child.clone()) {
					queue.push_back(child.clone());
				}
			}
		}
	}

	for inv in &invalid {
		in_degree.remove(inv);
	}

	// NOTE: A Vec + explicit sort is intentional here. `/get_missing_events`
	// responses are capped at LIMIT_MAX, so the frontier stays tiny and this is
	// simpler than maintaining a BinaryHeap with reversed ordering semantics.
	let mut zero_in_degree: Vec<OwnedEventId> = in_degree
		.iter()
		.filter(|(_, degree)| **degree == 0)
		.map(|(event_id, _)| event_id.clone())
		.collect();

	sort_topological_frontier(&mut zero_in_degree, &depth_map);

	let mut ordered = Vec::with_capacity(in_degree.len());
	while let Some(event_id) = zero_in_degree.pop() {
		ordered.push(event_id.clone());

		if let Some(children) = graph.get(&event_id) {
			for child in children {
				if let Some(degree) = in_degree.get_mut(child) {
					*degree = degree.saturating_sub(1);
					if *degree == 0 {
						zero_in_degree.push(child.clone());
					}
				}
			}

			sort_topological_frontier(&mut zero_in_degree, &depth_map);
		}
	}

	if ordered.len() < in_degree.len() {
		let placed: HashSet<&OwnedEventId> = ordered.iter().collect();
		let mut remaining: Vec<OwnedEventId> = in_degree
			.keys()
			.filter(|event_id| !placed.contains(event_id))
			.cloned()
			.collect();

		sort_topological_frontier(&mut remaining, &depth_map);
		remaining.reverse();
		ordered.extend(remaining);
	}

	ordered
}

fn sort_topological_frontier(
	frontier: &mut [OwnedEventId],
	depth_map: &HashMap<OwnedEventId, UInt>,
) {
	frontier.sort_by(|left, right| {
		let left_depth = depth_map
			.get(left)
			.copied()
			.unwrap_or_else(UInt::default);
		let right_depth = depth_map
			.get(right)
			.copied()
			.unwrap_or_else(UInt::default);

		right_depth
			.cmp(&left_depth)
			.then_with(|| right.cmp(left))
	});
}

#[cfg(test)]
mod tests {
	use ruma::{OwnedEventId, UInt};

	use super::topo_sort_events;

	fn event_id(id: &str) -> OwnedEventId { format!("${id}:example.com").try_into().unwrap() }

	fn depth(depth: u64) -> UInt { UInt::new(depth).unwrap() }

	#[test]
	fn topo_sort_orders_linear_chain_oldest_first() {
		let a = event_id("a");
		let b = event_id("b");
		let c = event_id("c");

		let mut reached_events = std::collections::HashSet::new();
		reached_events.insert(event_id("root"));

		let sorted = topo_sort_events(
			vec![
				(c.clone(), vec![b.clone()], depth(3)),
				(b.clone(), vec![a.clone()], depth(2)),
				(a.clone(), vec![event_id("root")], depth(1)),
			],
			&reached_events,
			UInt::new(0).unwrap(),
		);

		assert_eq!(sorted, vec![a, b, c]);
	}

	#[test]
	fn topo_sort_orders_fork_merge_oldest_first() {
		let a = event_id("a");
		let b = event_id("b");
		let c = event_id("c");
		let d = event_id("d");

		let mut reached_events = std::collections::HashSet::new();
		reached_events.insert(event_id("root"));

		let sorted = topo_sort_events(
			vec![
				(a.clone(), vec![event_id("root")], depth(1)),
				(b.clone(), vec![a.clone()], depth(2)),
				(c.clone(), vec![a.clone()], depth(2)),
				(d.clone(), vec![b.clone(), c.clone()], depth(3)),
			],
			&reached_events,
			UInt::new(0).unwrap(),
		);

		assert_eq!(sorted, vec![a, b, c, d]);
	}

	#[test]
	fn topo_sort_cycle_fallback_keeps_all_events() {
		let a = event_id("a");
		let b = event_id("b");

		let reached_events = std::collections::HashSet::new();

		let sorted = topo_sort_events(
			vec![(a.clone(), vec![b.clone()], depth(1)), (b.clone(), vec![a.clone()], depth(2))],
			&reached_events,
			UInt::new(0).unwrap(),
		);

		assert_eq!(sorted.len(), 2);
		assert!(sorted.contains(&a));
		assert!(sorted.contains(&b));
	}

	/// Regression test for 17ef683d4: a prev-event can be *walked* (and thus
	/// known-reached) without appearing in the returned batch, e.g. it was
	/// filtered out by `min_depth` or visibility. Such a prev must not
	/// invalidate its child just because it isn't one of the events being
	/// topo-sorted.
	#[test]
	fn topo_sort_reached_but_excluded_prev_is_not_invalidated() {
		let x = event_id("x");
		let y = event_id("y");

		// `x` was walked (so it's in `reached_events`/`seen`) but got filtered
		// out of the batch before reaching topo_sort_events, so it never
		// appears as a key in `events`.
		let mut reached_events = std::collections::HashSet::new();
		reached_events.insert(x.clone());

		let sorted = topo_sort_events(
			vec![(y.clone(), vec![x], depth(2))],
			&reached_events,
			UInt::new(0).unwrap(),
		);

		assert_eq!(sorted, vec![y]);
	}

	/// Regression test for bbe1e7238: an event sitting exactly at
	/// `min_depth` whose prev is missing must not be invalidated, since that
	/// prev is expected to be below the requested boundary and simply won't
	/// be sent. The check is `depth > min_depth`, not `>=`.
	#[test]
	fn topo_sort_event_at_min_depth_boundary_is_not_invalidated() {
		let a = event_id("a");
		let missing = event_id("missing");

		let reached_events = std::collections::HashSet::new();
		let min_depth = UInt::new(5).unwrap();

		let sorted = topo_sort_events(
			vec![(a.clone(), vec![missing], depth(5))],
			&reached_events,
			min_depth,
		);

		assert_eq!(sorted, vec![a]);
	}

	/// Companion to the boundary test above: one depth past the boundary,
	/// with the same unreached/missing prev, must still be invalidated.
	#[test]
	fn topo_sort_event_past_min_depth_boundary_is_invalidated() {
		let a = event_id("a");
		let missing = event_id("missing");

		let reached_events = std::collections::HashSet::new();
		let min_depth = UInt::new(5).unwrap();

		let sorted =
			topo_sort_events(vec![(a, vec![missing], depth(6))], &reached_events, min_depth);

		assert!(sorted.is_empty());
	}

	/// Invalidation must cascade to descendants: a child whose own direct
	/// prev is present in the batch should still be dropped if that prev
	/// was itself invalidated.
	#[test]
	fn topo_sort_invalidation_cascades_to_descendants() {
		let a = event_id("a");
		let b = event_id("b");
		let missing = event_id("missing");

		let reached_events = std::collections::HashSet::new();

		let sorted = topo_sort_events(
			vec![
				// `a`'s prev is missing and unreached -> `a` is invalid.
				(a.clone(), vec![missing], depth(1)),
				// `b`'s only prev, `a`, is present in the batch, so `b`
				// isn't directly invalid -- it must be caught by cascade.
				(b.clone(), vec![a.clone()], depth(2)),
			],
			&reached_events,
			UInt::new(0).unwrap(),
		);

		assert!(!sorted.contains(&a));
		assert!(!sorted.contains(&b));
	}

	/// Defensive test for a malformed/duplicated `prev_events` list (the
	/// same prev referenced twice by one event). This inflates in-degree by
	/// two for a single real edge, which can leave the node permanently
	/// non-zero and push it into the fallback path; it must still terminate
	/// and return every event exactly once.
	#[test]
	fn topo_sort_duplicate_prev_event_entries_still_terminate() {
		let a = event_id("a");
		let b = event_id("b");

		let mut reached_events = std::collections::HashSet::new();
		reached_events.insert(event_id("root"));

		let sorted = topo_sort_events(
			vec![
				(a.clone(), vec![event_id("root")], depth(1)),
				(b.clone(), vec![a.clone(), a.clone()], depth(2)),
			],
			&reached_events,
			UInt::new(0).unwrap(),
		);

		assert_eq!(sorted.len(), 2);
		assert!(sorted.contains(&a));
		assert!(sorted.contains(&b));
	}
}
