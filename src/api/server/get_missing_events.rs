use std::collections::{HashMap, HashSet, VecDeque};

use axum::extract::State;
use ruma::{OwnedEventId, UInt, api::federation::event::get_missing_events};
use tuwunel_core::{Result, debug};

use super::AccessCheck;
use crate::Ruma;

/// arbitrary number but synapse's is 20 and we can handle lots of these anyways
const LIMIT_MAX: usize = 50;
/// spec says default is 10
const LIMIT_DEFAULT: usize = 10;
/// Bound predecessor traversal independently from the response size so omitted
/// events cannot force a single request to scan arbitrarily deep room history.
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
		.await
		.ok();

	let mut queue: VecDeque<OwnedEventId> = body.latest_events.iter().cloned().collect();
	let mut seen: HashSet<OwnedEventId> = body.earliest_events.iter().cloned().collect();
	let mut results: Vec<(OwnedEventId, Vec<OwnedEventId>, UInt, _)> = Vec::with_capacity(limit);
	let mut traversed = 0_usize;

	while let Some(event_id) = queue.pop_front() {
		if !seen.insert(event_id.clone()) {
			continue;
		}

		if traversed >= WALK_LIMIT_MAX {
			debug!(
				?body.origin,
				room_id = %body.room_id,
				traversed,
				limit = WALK_LIMIT_MAX,
				"Stopping get_missing_events traversal after reaching predecessor walk limit"
			);
			break;
		}

		traversed = traversed.saturating_add(1);

		let Ok(pdu) = services.timeline.get_pdu(&event_id).await else {
			debug!(?body.origin, %event_id, "Event does not exist locally, skipping");
			continue;
		};

		if pdu.depth > body.min_depth {
			queue.extend(pdu.prev_events.iter().cloned());
		}

		if body.latest_events.contains(&event_id) {
			continue;
		}

		if pdu.depth < body.min_depth {
			continue;
		}

		if !services
			.state_accessor
			.server_can_see_event(body.origin(), &body.room_id, &event_id)
			.await
		{
			debug!(
				?body.origin,
				%event_id,
				room_id = %body.room_id,
				"Server cannot see event, traversing through it but omitting it from the response"
			);
			continue;
		}

		let Ok(event) = services.timeline.get_pdu_json(&event_id).await else {
			debug!(?body.origin, %event_id, "Event JSON does not exist locally, skipping");
			continue;
		};

		let event = services
			.state_accessor
			.erased_for_server(body.origin(), event)
			.await;

		let event = services
			.federation
			.format_pdu_into(event, room_version.as_ref())
			.await;

		results.push((event_id, pdu.prev_events.into_vec(), pdu.depth, event));

		if results.len() >= limit {
			break;
		}
	}

	let sorted_ids = topo_sort_events(
		results
			.iter()
			.map(|(event_id, prev_events, depth, _)| {
				(event_id.clone(), prev_events.clone(), *depth)
			}),
	);

	let mut event_map: HashMap<OwnedEventId, _> = results
		.into_iter()
		.map(|(event_id, _, _, event)| (event_id, event))
		.collect();

	let events = sorted_ids
		.into_iter()
		.filter_map(|event_id| event_map.remove(&event_id))
		.collect();

	Ok(get_missing_events::v1::Response { events })
}

fn topo_sort_events(
	events: impl IntoIterator<Item = (OwnedEventId, Vec<OwnedEventId>, UInt)>,
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

	for (event_id, prev_events, _) in events {
		for prev_event in prev_events {
			if in_degree.contains_key(&prev_event) {
				graph
					.entry(prev_event)
					.or_default()
					.push(event_id.clone());
				let degree = in_degree
					.get_mut(&event_id)
					.expect("event must be present in in_degree");
				*degree = degree.checked_add(1).expect("in-degree overflow");
			}
		}
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
	use ruma::OwnedEventId;

	use super::topo_sort_events;

	fn event_id(id: &str) -> OwnedEventId { format!("${id}:example.com").try_into().unwrap() }

	fn depth(depth: u64) -> ruma::UInt { ruma::UInt::new(depth).unwrap() }

	#[test]
	fn topo_sort_orders_linear_chain_oldest_first() {
		let a = event_id("a");
		let b = event_id("b");
		let c = event_id("c");

		let sorted = topo_sort_events(vec![
			(c.clone(), vec![b.clone()], depth(3)),
			(b.clone(), vec![a.clone()], depth(2)),
			(a.clone(), vec![event_id("root")], depth(1)),
		]);

		assert_eq!(sorted, vec![a, b, c]);
	}

	#[test]
	fn topo_sort_orders_fork_merge_oldest_first() {
		let a = event_id("a");
		let b = event_id("b");
		let c = event_id("c");
		let d = event_id("d");

		let sorted = topo_sort_events(vec![
			(a.clone(), vec![event_id("root")], depth(1)),
			(b.clone(), vec![a.clone()], depth(2)),
			(c.clone(), vec![a.clone()], depth(2)),
			(d.clone(), vec![b.clone(), c.clone()], depth(3)),
		]);

		assert_eq!(sorted, vec![a, b, c, d]);
	}
}
