#![cfg_attr(test, allow(warnings))]

pub(crate) mod error;
pub mod event_auth;
mod power_levels;
mod room_version;

#[cfg(test)]
mod test_utils;

#[cfg(test)]
mod benches;

use std::{
	borrow::Borrow,
	cmp::{Ordering, Reverse},
	collections::{BinaryHeap, HashMap, HashSet},
	hash::{BuildHasher, Hash},
};

use futures::{Future, FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future};
use ruma::{
	EventId, Int, MilliSecondsSinceUnixEpoch, RoomVersionId,
	events::{
		StateEventType, TimelineEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	int,
};
use serde_json::from_str as from_json_str;

pub(crate) use self::error::Error;
use self::power_levels::PowerLevelsContentFields;
pub use self::{
	event_auth::{auth_check, auth_types_for_event},
	room_version::RoomVersion,
};
use crate::{
	debug, debug_error,
	matrix::{event::Event, pdu::StateKey},
	trace,
	utils::stream::{
		BroadbandExt, IterStream, ReadyExt, TryBroadbandExt, TryReadyExt, WidebandExt,
	},
	warn,
};

/// A mapping of event type and state_key to some value `T`, usually an
/// `EventId`.
pub type StateMap<T> = HashMap<TypeStateKey, T>;
pub type StateMapItem<T> = (TypeStateKey, T);
pub type TypeStateKey = (StateEventType, StateKey);

type Result<T, E = Error> = crate::Result<T, E>;

/// Resolve sets of state events as they come in.
///
/// Internally `StateResolution` builds a graph and an auth chain to allow for
/// state conflict resolution.
///
/// ## Arguments
///
/// * `state_sets` - The incoming state to resolve. Each `StateMap` represents a
///   possible fork in the state of a room.
///
/// * `auth_chain_sets` - The full recursive set of `auth_events` for each event
///   in the `state_sets`.
///
/// * `event_fetch` - Any event not found in the `event_map` will defer to this
///   closure to find the event.
///
/// * `parallel_fetches` - The number of asynchronous fetch requests in-flight
///   for any given operation.
///
/// ## Invariants
///
/// The caller of `resolve` must ensure that all the events are from the same
/// room. Although this function takes a `RoomId` it does not check that each
/// event is part of the same room.
//#[tracing::instrument(level = "debug", skip(state_sets, auth_chain_sets,
//#[tracing::instrument(level event_fetch))]
pub async fn resolve<'a, E, Sets, SetIter, Hasher, Fetch, FetchFut, Exists, ExistsFut>(
	room_version: &RoomVersionId,
	state_sets: Sets,
	auth_chain_sets: &'a [HashSet<E::Id, Hasher>],
	event_fetch: &Fetch,
	event_exists: &Exists,
	parallel_fetches: usize,
) -> Result<StateMap<E::Id>>
where
	Fetch: Fn(E::Id) -> FetchFut + Sync,
	FetchFut: Future<Output = Option<E>> + Send,
	Exists: Fn(E::Id) -> ExistsFut + Sync,
	ExistsFut: Future<Output = bool> + Send,
	Sets: IntoIterator<IntoIter = SetIter> + Send,
	SetIter: Iterator<Item = &'a StateMap<E::Id>> + Clone + Send,
	Hasher: BuildHasher + Send + Sync,
	E: Event + Clone + Send + Sync,
	E::Id: Borrow<EventId> + Send + Sync,
	for<'b> &'b E: Send,
{
	debug!("State resolution starting");

	// Split non-conflicting and conflicting state
	let (clean, conflicting) = separate(state_sets.into_iter());

	debug!(count = clean.len(), "non-conflicting events");
	trace!(map = ?clean, "non-conflicting events");

	if conflicting.is_empty() {
		debug!("no conflicting state found");
		return Ok(clean);
	}

	debug!(count = conflicting.len(), "conflicting events");
	trace!(map = ?conflicting, "conflicting events");

	let conflicting_values = conflicting.into_values().flatten().stream();

	// `all_conflicted` contains unique items
	// synapse says `full_set = {eid for eid in full_conflicted_set if eid in
	// event_map}`
	let all_conflicted: HashSet<_> = get_auth_chain_diff(auth_chain_sets)
		.chain(conflicting_values)
		.broad_filter_map(async |id| event_exists(id.clone()).await.then_some(id))
		.collect()
		.await;

	debug!(count = all_conflicted.len(), "full conflicted set");
	trace!(set = ?all_conflicted, "full conflicted set");

	// We used to check that all events are events from the correct room
	// this is now a check the caller of `resolve` must make.

	// Get only the control events with a state_key: "" or ban/kick event (sender !=
	// state_key)
	let control_events: Vec<_> = all_conflicted
		.iter()
		.stream()
		.wide_filter_map(async |id| {
			is_power_event_id(id, &event_fetch)
				.await
				.then_some(id.clone())
		})
		.collect()
		.await;

	// Sort the control events based on power_level/clock/event_id and
	// outgoing/incoming edges
	let sorted_control_levels = reverse_topological_power_sort(
		control_events,
		&all_conflicted,
		&event_fetch,
		parallel_fetches,
	)
	.await?;

	debug!(count = sorted_control_levels.len(), "power events");
	trace!(list = ?sorted_control_levels, "sorted power events");

	let room_version = RoomVersion::new(room_version)?;
	// Sequentially auth check each control event.
	let resolved_control = iterative_auth_check(
		&room_version,
		sorted_control_levels.iter().stream(),
		clean.clone(),
		&event_fetch,
	)
	.await?;

	debug!(count = resolved_control.len(), "resolved power events");
	trace!(map = ?resolved_control, "resolved power events");

	// At this point the control_events have been resolved we now have to
	// sort the remaining events using the mainline of the resolved power level.
	let deduped_power_ev: HashSet<_> = sorted_control_levels.into_iter().collect();

	// This removes the control events that passed auth and more importantly those
	// that failed auth
	let events_to_resolve: Vec<_> = all_conflicted
		.iter()
		.filter(|&id| !deduped_power_ev.contains(id.borrow()))
		.cloned()
		.collect();

	debug!(count = events_to_resolve.len(), "events left to resolve");
	trace!(list = ?events_to_resolve, "events left to resolve");

	// This "epochs" power level event
	let power_levels_ty_sk = (StateEventType::RoomPowerLevels, StateKey::new());
	let power_event = resolved_control.get(&power_levels_ty_sk);

	debug!(event_id = ?power_event, "power event");

	let sorted_left_events =
		mainline_sort(&events_to_resolve, power_event.cloned(), &event_fetch).await?;

	trace!(list = ?sorted_left_events, "events left, sorted");

	let mut resolved_state = iterative_auth_check(
		&room_version,
		sorted_left_events.iter().stream(),
		resolved_control, // The control events are added to the final resolved state
		&event_fetch,
	)
	.await?;

	// Add unconflicted state to the resolved state
	// We priorities the unconflicting state
	resolved_state.extend(clean);

	debug!("state resolution finished");

	Ok(resolved_state)
}

/// Split the events that have no conflicts from those that are conflicting.
///
/// The return tuple looks like `(unconflicted, conflicted)`.
///
/// State is determined to be conflicting if for the given key (StateEventType,
/// StateKey) there is not exactly one event ID. This includes missing events,
/// if one state_set includes an event that none of the other have this is a
/// conflicting event.
fn separate<'a, Id>(
	state_sets_iter: impl Iterator<Item = &'a StateMap<Id>>,
) -> (StateMap<Id>, StateMap<Vec<Id>>)
where
	Id: Clone + Eq + Hash + 'a,
{
	let mut state_set_count: usize = 0;
	let mut occurrences = HashMap::<_, HashMap<_, _>>::new();

	let state_sets_iter =
		state_sets_iter.inspect(|_| state_set_count = state_set_count.saturating_add(1));
	for (k, v) in state_sets_iter.flatten() {
		occurrences
			.entry(k)
			.or_default()
			.entry(v)
			.and_modify(|x: &mut usize| *x = x.saturating_add(1))
			.or_insert(1);
	}

	let mut unconflicted_state = StateMap::new();
	let mut conflicted_state = StateMap::new();

	for (k, v) in occurrences {
		for (id, occurrence_count) in v {
			if occurrence_count == state_set_count {
				unconflicted_state.insert((k.0.clone(), k.1.clone()), id.clone());
			} else {
				conflicted_state
					.entry((k.0.clone(), k.1.clone()))
					.and_modify(|x: &mut Vec<_>| x.push(id.clone()))
					.or_insert_with(|| vec![id.clone()]);
			}
		}
	}

	(unconflicted_state, conflicted_state)
}

/// Returns a Vec of deduped EventIds that appear in some chains but not others.
#[allow(clippy::arithmetic_side_effects)]
fn get_auth_chain_diff<Id, Hasher>(
	auth_chain_sets: &[HashSet<Id, Hasher>],
) -> impl Stream<Item = Id> + Send + use<Id, Hasher>
where
	Id: Clone + Eq + Hash + Send,
	Hasher: BuildHasher + Send + Sync,
{
	let num_sets = auth_chain_sets.len();
	let mut id_counts: HashMap<Id, usize> = HashMap::new();
	for id in auth_chain_sets.iter().flatten() {
		*id_counts.entry(id.clone()).or_default() += 1;
	}

	id_counts
		.into_iter()
		.filter_map(move |(id, count)| (count < num_sets).then_some(id))
		.stream()
}

/// Events are sorted from "earliest" to "latest".
///
/// They are compared using the negative power level (reverse topological
/// ordering), the origin server timestamp and in case of a tie the `EventId`s
/// are compared lexicographically.
///
/// The power level is negative because a higher power level is equated to an
/// earlier (further back in time) origin server timestamp.
#[tracing::instrument(level = "debug", skip_all)]
async fn reverse_topological_power_sort<E, F, Fut>(
	events_to_sort: Vec<E::Id>,
	auth_diff: &HashSet<E::Id>,
	fetch_event: &F,
	parallel_fetches: usize,
) -> Result<Vec<E::Id>>
where
	F: Fn(E::Id) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Send + Sync,
	E::Id: Borrow<EventId> + Send + Sync,
{
	debug!("reverse topological sort of power events");

	let mut graph = HashMap::new();
	for event_id in events_to_sort {
		add_event_and_auth_chain_to_graph(&mut graph, event_id, auth_diff, fetch_event).await;
	}

	// This is used in the `key_fn` passed to the lexico_topo_sort fn
	let event_to_pl = graph
		.keys()
		.stream()
		.map(|event_id| {
			get_power_level_for_sender(event_id.clone(), fetch_event)
				.map(move |res| res.map(|pl| (event_id, pl)))
		})
		.buffer_unordered(parallel_fetches)
		.ready_try_fold(HashMap::new(), |mut event_to_pl, (event_id, pl)| {
			debug!(
				event_id = event_id.borrow().as_str(),
				power_level = i64::from(pl),
				"found the power level of an event's sender",
			);

			event_to_pl.insert(event_id.clone(), pl);
			Ok(event_to_pl)
		})
		.boxed()
		.await?;

	let event_to_pl = &event_to_pl;
	let fetcher = |event_id: E::Id| async move {
		let pl = *event_to_pl
			.get(event_id.borrow())
			.ok_or_else(|| Error::NotFound(String::new()))?;
		let ev = fetch_event(event_id)
			.await
			.ok_or_else(|| Error::NotFound(String::new()))?;
		Ok((pl, ev.origin_server_ts()))
	};

	lexicographical_topological_sort(&graph, &fetcher).await
}

/// Sorts the event graph based on number of outgoing/incoming edges.
///
/// `key_fn` is used as to obtain the power level and age of an event for
/// breaking ties (together with the event ID).
#[tracing::instrument(level = "debug", skip_all)]
pub async fn lexicographical_topological_sort<Id, F, Fut, Hasher>(
	graph: &HashMap<Id, HashSet<Id, Hasher>>,
	key_fn: &F,
) -> Result<Vec<Id>>
where
	F: Fn(Id) -> Fut + Sync,
	Fut: Future<Output = Result<(Int, MilliSecondsSinceUnixEpoch)>> + Send,
	Id: Borrow<EventId> + Clone + Eq + Hash + Ord + Send + Sync,
	Hasher: BuildHasher + Default + Clone + Send + Sync,
{
	#[derive(PartialEq, Eq)]
	struct TieBreaker<'a, Id> {
		power_level: Int,
		origin_server_ts: MilliSecondsSinceUnixEpoch,
		event_id: &'a Id,
	}

	impl<Id> Ord for TieBreaker<'_, Id>
	where
		Id: Ord,
	{
		fn cmp(&self, other: &Self) -> Ordering {
			// NOTE: the power level comparison is "backwards" intentionally.
			// See the "Mainline ordering" section of the Matrix specification
			// around where it says the following:
			//
			// > for events `x` and `y`, `x < y` if [...]
			//
			// <https://spec.matrix.org/v1.12/rooms/v11/#definitions>
			other
				.power_level
				.cmp(&self.power_level)
				.then(self.origin_server_ts.cmp(&other.origin_server_ts))
				.then(self.event_id.cmp(other.event_id))
		}
	}

	impl<Id> PartialOrd for TieBreaker<'_, Id>
	where
		Id: Ord,
	{
		fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
			Some(self.cmp(other))
		}
	}

	debug!("starting lexicographical topological sort");

	// NOTE: an event that has no incoming edges happened most recently,
	// and an event that has no outgoing edges happened least recently.

	// NOTE: this is basically Kahn's algorithm except we look at nodes with no
	// outgoing edges, c.f.
	// https://en.wikipedia.org/wiki/Topological_sorting#Kahn's_algorithm

	// outdegree_map is an event referring to the events before it, the
	// more outdegree's the more recent the event.
	let mut outdegree_map = graph.clone();

	// The number of events that depend on the given event (the EventId key)
	// How many events reference this event in the DAG as a parent
	let mut reverse_graph: HashMap<_, HashSet<_, Hasher>> = HashMap::new();

	// Vec of nodes that have zero out degree, least recent events.
	let mut zero_outdegree = Vec::new();

	for (node, edges) in graph {
		if edges.is_empty() {
			let (power_level, origin_server_ts) = key_fn(node.clone()).await?;
			// The `Reverse` is because rusts `BinaryHeap` sorts largest -> smallest we need
			// smallest -> largest
			zero_outdegree.push(Reverse(TieBreaker {
				power_level,
				origin_server_ts,
				event_id: node,
			}));
		}

		reverse_graph.entry(node).or_default();
		for edge in edges {
			reverse_graph
				.entry(edge)
				.or_default()
				.insert(node);
		}
	}

	let mut heap = BinaryHeap::from(zero_outdegree);

	// We remove the oldest node (most incoming edges) and check against all other
	let mut sorted = vec![];
	// Destructure the `Reverse` and take the smallest `node` each time
	while let Some(Reverse(item)) = heap.pop() {
		let node = item.event_id;

		for &parent in reverse_graph
			.get(node)
			.expect("EventId in heap is also in reverse_graph")
		{
			// The number of outgoing edges this node has
			let out = outdegree_map
				.get_mut(parent.borrow())
				.expect("outdegree_map knows of all referenced EventIds");

			// Only push on the heap once older events have been cleared
			out.remove(node.borrow());
			if out.is_empty() {
				let (power_level, origin_server_ts) = key_fn(parent.clone()).await?;
				heap.push(Reverse(TieBreaker {
					power_level,
					origin_server_ts,
					event_id: parent,
				}));
			}
		}

		// synapse yields we push then return the vec
		sorted.push(node.clone());
	}

	Ok(sorted)
}

/// Find the power level for the sender of `event_id` or return a default value
/// of zero.
///
/// Do NOT use this any where but topological sort, we find the power level for
/// the eventId at the eventId's generation (we walk backwards to `EventId`s
/// most recent previous power level event).
async fn get_power_level_for_sender<E, F, Fut>(
	event_id: E::Id,
	fetch_event: &F,
) -> serde_json::Result<Int>
where
	F: Fn(E::Id) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Send,
	E::Id: Borrow<EventId> + Send,
{
	debug!("fetch event ({event_id}) senders power level");

	let event = fetch_event(event_id).await;

	let auth_events = event.as_ref().map(Event::auth_events);

	let pl = auth_events
		.into_iter()
		.flatten()
		.stream()
		.broadn_filter_map(5, |aid| fetch_event(aid.clone()))
		.ready_find(|aev| is_type_and_key(aev, &TimelineEventType::RoomPowerLevels, ""))
		.await;

	let content: PowerLevelsContentFields = match pl {
		| None => return Ok(int!(0)),
		| Some(ev) => from_json_str(ev.content().get())?,
	};

	if let Some(ev) = event {
		if let Some(&user_level) = content.get_user_power(ev.sender()) {
			debug!("found {} at power_level {user_level}", ev.sender());
			return Ok(user_level);
		}
	}

	Ok(content.users_default)
}

/// Check the that each event is authenticated based on the events before it.
///
/// ## Returns
///
/// The `unconflicted_state` combined with the newly auth'ed events. So any
/// event that fails the `event_auth::auth_check` will be excluded from the
/// returned state map.
///
/// For each `events_to_check` event we gather the events needed to auth it from
/// the the `fetch_event` closure and verify each event using the
/// `event_auth::auth_check` function.
async fn iterative_auth_check<'a, E, F, Fut, S>(
	room_version: &RoomVersion,
	events_to_check: S,
	unconflicted_state: StateMap<E::Id>,
	fetch_event: &F,
) -> Result<StateMap<E::Id>>
where
	F: Fn(E::Id) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E::Id: Borrow<EventId> + Clone + Eq + Ord + Send + Sync + 'a,
	S: Stream<Item = &'a E::Id> + Send + 'a,
	E: Event + Clone + Send + Sync,
{
	debug!("starting iterative auth check");

	let events_to_check: Vec<_> = events_to_check
		.map(Result::Ok)
		.broad_and_then(async |event_id| {
			fetch_event(event_id.clone())
				.await
				.ok_or_else(|| Error::NotFound(format!("Failed to find {event_id}")))
		})
		.try_collect()
		.boxed()
		.await?;

	let auth_event_ids: HashSet<E::Id> = events_to_check
		.iter()
		.flat_map(|event: &E| event.auth_events().map(Clone::clone))
		.collect();

	let auth_events: HashMap<E::Id, E> = auth_event_ids
		.into_iter()
		.stream()
		.broad_filter_map(fetch_event)
		.map(|auth_event| (auth_event.event_id().clone(), auth_event))
		.collect()
		.boxed()
		.await;

	let auth_events = &auth_events;
	let mut resolved_state = unconflicted_state;
	for event in &events_to_check {
		let state_key = event
			.state_key()
			.ok_or_else(|| Error::InvalidPdu("State event had no state key".to_owned()))?;

		let auth_types = auth_types_for_event(
			event.event_type(),
			event.sender(),
			Some(state_key),
			event.content(),
		)?;

		let mut auth_state = StateMap::new();
		for aid in event.auth_events() {
			if let Some(ev) = auth_events.get(aid.borrow()) {
				//TODO: synapse checks "rejected_reason" which is most likely related to
				// soft-failing
				auth_state.insert(
					ev.event_type()
						.with_state_key(ev.state_key().ok_or_else(|| {
							Error::InvalidPdu("State event had no state key".to_owned())
						})?),
					ev.clone(),
				);
			} else {
				warn!(event_id = aid.borrow().as_str(), "missing auth event");
			}
		}

		auth_types
			.iter()
			.stream()
			.ready_filter_map(|key| Some((key, resolved_state.get(key)?)))
			.filter_map(|(key, ev_id)| async move {
				if let Some(event) = auth_events.get(ev_id.borrow()) {
					Some((key, event.clone()))
				} else {
					Some((key, fetch_event(ev_id.clone()).await?))
				}
			})
			.ready_for_each(|(key, event)| {
				//TODO: synapse checks "rejected_reason" is None here
				auth_state.insert(key.to_owned(), event);
			})
			.await;

		debug!("event to check {:?}", event.event_id());

		// The key for this is (eventType + a state_key of the signed token not sender)
		// so search for it
		let current_third_party = auth_state.iter().find_map(|(_, pdu)| {
			(*pdu.event_type() == TimelineEventType::RoomThirdPartyInvite).then_some(pdu)
		});

		let fetch_state = |ty: &StateEventType, key: &str| {
			future::ready(auth_state.get(&ty.with_state_key(key)))
		};

		let auth_result =
			auth_check(room_version, &event, current_third_party.as_ref(), fetch_state).await;

		match auth_result {
			| Ok(true) => {
				// add event to resolved state map
				resolved_state.insert(
					event.event_type().with_state_key(state_key),
					event.event_id().clone(),
				);
			},
			| Ok(false) => {
				// synapse passes here on AuthError. We do not add this event to resolved_state.
				warn!("event {} failed the authentication check", event.event_id());
			},
			| Err(e) => {
				debug_error!("event {} failed the authentication check: {e}", event.event_id());
				return Err(e);
			},
		}
	}

	Ok(resolved_state)
}

/// Returns the sorted `to_sort` list of `EventId`s based on a mainline sort
/// using the depth of `resolved_power_level`, the server timestamp, and the
/// eventId.
///
/// The depth of the given event is calculated based on the depth of it's
/// closest "parent" power_level event. If there have been two power events the
/// after the most recent are depth 0, the events before (with the first power
/// level as a parent) will be marked as depth 1. depth 1 is "older" than depth
/// 0.
async fn mainline_sort<E, F, Fut>(
	to_sort: &[E::Id],
	resolved_power_level: Option<E::Id>,
	fetch_event: &F,
) -> Result<Vec<E::Id>>
where
	F: Fn(E::Id) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Clone + Send + Sync,
	E::Id: Borrow<EventId> + Clone + Send + Sync,
{
	debug!("mainline sort of events");

	// There are no EventId's to sort, bail.
	if to_sort.is_empty() {
		return Ok(vec![]);
	}

	let mut mainline = vec![];
	let mut pl = resolved_power_level;
	while let Some(p) = pl {
		mainline.push(p.clone());

		let event = fetch_event(p.clone())
			.await
			.ok_or_else(|| Error::NotFound(format!("Failed to find {p}")))?;

		pl = None;
		for aid in event.auth_events() {
			let ev = fetch_event(aid.clone())
				.await
				.ok_or_else(|| Error::NotFound(format!("Failed to find {aid}")))?;

			if is_type_and_key(&ev, &TimelineEventType::RoomPowerLevels, "") {
				pl = Some(aid.to_owned());
				break;
			}
		}
	}

	let mainline_map: HashMap<_, _> = mainline
		.iter()
		.rev()
		.enumerate()
		.map(|(idx, eid)| ((*eid).clone(), idx))
		.collect();

	let order_map: HashMap<_, _> = to_sort
		.iter()
		.stream()
		.broad_filter_map(async |ev_id| {
			fetch_event(ev_id.clone())
				.await
				.map(|event| (event, ev_id))
		})
		.broad_filter_map(|(event, ev_id)| {
			get_mainline_depth(Some(event.clone()), &mainline_map, fetch_event)
				.map_ok(move |depth| (ev_id, (depth, event.origin_server_ts(), ev_id)))
				.map(Result::ok)
		})
		.collect()
		.boxed()
		.await;

	// Sort the event_ids by their depth, timestamp and EventId
	// unwrap is OK order map and sort_event_ids are from to_sort (the same Vec)
	let mut sort_event_ids: Vec<_> = order_map.keys().map(|&k| k.clone()).collect();

	sort_event_ids.sort_by_key(|sort_id| &order_map[sort_id]);

	Ok(sort_event_ids)
}

/// Get the mainline depth from the `mainline_map` or finds a power_level event
/// that has an associated mainline depth.
async fn get_mainline_depth<E, F, Fut>(
	mut event: Option<E>,
	mainline_map: &HashMap<E::Id, usize>,
	fetch_event: &F,
) -> Result<usize>
where
	F: Fn(E::Id) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Send + Sync,
	E::Id: Borrow<EventId> + Send + Sync,
{
	while let Some(sort_ev) = event {
		debug!(event_id = sort_ev.event_id().borrow().as_str(), "mainline");

		let id = sort_ev.event_id();
		if let Some(depth) = mainline_map.get(id.borrow()) {
			return Ok(*depth);
		}

		event = None;
		for aid in sort_ev.auth_events() {
			let aev = fetch_event(aid.clone())
				.await
				.ok_or_else(|| Error::NotFound(format!("Failed to find {aid}")))?;

			if is_type_and_key(&aev, &TimelineEventType::RoomPowerLevels, "") {
				event = Some(aev);
				break;
			}
		}
	}
	// Did not find a power level event so we default to zero
	Ok(0)
}

async fn add_event_and_auth_chain_to_graph<E, F, Fut>(
	graph: &mut HashMap<E::Id, HashSet<E::Id>>,
	event_id: E::Id,
	auth_diff: &HashSet<E::Id>,
	fetch_event: &F,
) where
	F: Fn(E::Id) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Send + Sync,
	E::Id: Borrow<EventId> + Clone + Send + Sync,
{
	let mut state = vec![event_id];
	while let Some(eid) = state.pop() {
		graph.entry(eid.clone()).or_default();
		let event = fetch_event(eid.clone()).await;
		let auth_events = event
			.as_ref()
			.map(Event::auth_events)
			.into_iter()
			.flatten();

		// Prefer the store to event as the store filters dedups the events
		for aid in auth_events {
			if auth_diff.contains(aid.borrow()) {
				if !graph.contains_key(aid.borrow()) {
					state.push(aid.to_owned());
				}

				// We just inserted this at the start of the while loop
				graph
					.get_mut(eid.borrow())
					.unwrap()
					.insert(aid.to_owned());
			}
		}
	}
}

async fn is_power_event_id<E, F, Fut>(event_id: &E::Id, fetch: &F) -> bool
where
	F: Fn(E::Id) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Send,
	E::Id: Borrow<EventId> + Send + Sync,
{
	match fetch(event_id.clone()).await.as_ref() {
		| Some(state) => is_power_event(state),
		| _ => false,
	}
}

fn is_type_and_key(ev: impl Event, ev_type: &TimelineEventType, state_key: &str) -> bool {
	ev.event_type() == ev_type && ev.state_key() == Some(state_key)
}

fn is_power_event(event: impl Event) -> bool {
	match event.event_type() {
		| TimelineEventType::RoomPowerLevels
		| TimelineEventType::RoomJoinRules
		| TimelineEventType::RoomCreate => event.state_key() == Some(""),
		| TimelineEventType::RoomMember => {
			if let Ok(content) = from_json_str::<RoomMemberEventContent>(event.content().get()) {
				if [MembershipState::Leave, MembershipState::Ban].contains(&content.membership) {
					return Some(event.sender().as_str()) != event.state_key();
				}
			}

			false
		},
		| _ => false,
	}
}

/// Convenience trait for adding event type plus state key to state maps.
pub trait EventTypeExt {
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey);
}

impl EventTypeExt for StateEventType {
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey) {
		(self, state_key.into())
	}
}

impl EventTypeExt for TimelineEventType {
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey) {
		(self.into(), state_key.into())
	}
}

impl<T> EventTypeExt for &T
where
	T: EventTypeExt + Clone,
{
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey) {
		self.to_owned().with_state_key(state_key)
	}
}

#[cfg(test)]
mod tests {
	use std::collections::{HashMap, HashSet};

	use maplit::{hashmap, hashset};
	use rand::seq::SliceRandom;
	use ruma::{
		MilliSecondsSinceUnixEpoch, OwnedEventId, RoomVersionId,
		events::{
			StateEventType, TimelineEventType,
			room::join_rules::{JoinRule, RoomJoinRulesEventContent},
		},
		int, uint,
	};
	use serde_json::{json, value::to_raw_value as to_raw_json_value};

	use super::{
		Event, EventTypeExt, StateMap, is_power_event,
		room_version::RoomVersion,
		test_utils::{
			INITIAL_EVENTS, PduEvent, TestStore, alice, bob, charlie, do_check, ella, event_id,
			member_content_ban, member_content_join, room_id, to_init_pdu_event, to_pdu_event,
			zara,
		},
	};
	use crate::{debug, utils::stream::IterStream};

	async fn test_event_sort() {
		use futures::future::ready;

		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);
		let events = INITIAL_EVENTS();

		let event_map = events
			.values()
			.map(|ev| {
				(
					ev.event_type()
						.with_state_key(ev.state_key().unwrap()),
					ev.clone(),
				)
			})
			.collect::<StateMap<_>>();

		let auth_chain: HashSet<OwnedEventId> = HashSet::new();

		let power_events = event_map
			.values()
			.filter(|&pdu| is_power_event(&*pdu))
			.map(|pdu| pdu.event_id.clone())
			.collect::<Vec<_>>();

		let fetcher = |id| ready(events.get(&id).cloned());
		let sorted_power_events =
			super::reverse_topological_power_sort(power_events, &auth_chain, &fetcher, 1)
				.await
				.unwrap();

		let resolved_power = super::iterative_auth_check(
			&RoomVersion::V6,
			sorted_power_events.iter().stream(),
			HashMap::new(), // unconflicted events
			&fetcher,
		)
		.await
		.expect("iterative auth check failed on resolved events");

		// don't remove any events so we know it sorts them all correctly
		let mut events_to_sort = events.keys().cloned().collect::<Vec<_>>();

		events_to_sort.shuffle(&mut rand::thread_rng());

		let power_level = resolved_power
			.get(&(StateEventType::RoomPowerLevels, "".into()))
			.cloned();

		let sorted_event_ids = super::mainline_sort(&events_to_sort, power_level, &fetcher)
			.await
			.unwrap();

		assert_eq!(
			vec![
				"$CREATE:foo",
				"$IMA:foo",
				"$IPOWER:foo",
				"$IJR:foo",
				"$IMB:foo",
				"$IMC:foo",
				"$START:foo",
				"$END:foo"
			],
			sorted_event_ids
				.iter()
				.map(|id| id.to_string())
				.collect::<Vec<_>>()
		);
	}

	#[tokio::test]
	async fn test_sort() {
		for _ in 0..20 {
			// since we shuffle the eventIds before we sort them introducing randomness
			// seems like we should test this a few times
			test_event_sort().await;
		}
	}

	#[tokio::test]
	async fn ban_vs_power_level() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);

		let events = &[
			to_init_pdu_event(
				"PA",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"MA",
				alice(),
				TimelineEventType::RoomMember,
				Some(alice().to_string().as_str()),
				member_content_join(),
			),
			to_init_pdu_event(
				"MB",
				alice(),
				TimelineEventType::RoomMember,
				Some(bob().to_string().as_str()),
				member_content_ban(),
			),
			to_init_pdu_event(
				"PB",
				bob(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
		];

		let edges = vec![vec!["END", "MB", "MA", "PA", "START"], vec!["END", "PA", "PB"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec!["PA", "MA", "MB"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn topic_basic() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);

		let events = &[
			to_init_pdu_event(
				"T1",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"PA1",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"T2",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"PA2",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 0 } })).unwrap(),
			),
			to_init_pdu_event(
				"PB",
				bob(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"T3",
				bob(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
		];

		let edges =
			vec![vec!["END", "PA2", "T2", "PA1", "T1", "START"], vec!["END", "T3", "PB", "PA1"]]
				.into_iter()
				.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
				.collect::<Vec<_>>();

		let expected_state_ids = vec!["PA2", "T2"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn topic_reset() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);

		let events = &[
			to_init_pdu_event(
				"T1",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"PA",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"T2",
				bob(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"MB",
				alice(),
				TimelineEventType::RoomMember,
				Some(bob().to_string().as_str()),
				member_content_ban(),
			),
		];

		let edges = vec![vec!["END", "MB", "T2", "PA", "T1", "START"], vec!["END", "T1"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec!["T1", "MB", "PA"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn join_rule_evasion() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);

		let events = &[
			to_init_pdu_event(
				"JR",
				alice(),
				TimelineEventType::RoomJoinRules,
				Some(""),
				to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Private)).unwrap(),
			),
			to_init_pdu_event(
				"ME",
				ella(),
				TimelineEventType::RoomMember,
				Some(ella().to_string().as_str()),
				member_content_join(),
			),
		];

		let edges = vec![vec!["END", "JR", "START"], vec!["END", "ME", "START"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec![event_id("JR")];

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn offtopic_power_level() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);

		let events = &[
			to_init_pdu_event(
				"PA",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"PB",
				bob(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(
					&json!({ "users": { alice(): 100, bob(): 50, charlie(): 50 } }),
				)
				.unwrap(),
			),
			to_init_pdu_event(
				"PC",
				charlie(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50, charlie(): 0 } }))
					.unwrap(),
			),
		];

		let edges = vec![vec!["END", "PC", "PB", "PA", "START"], vec!["END", "PA"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec!["PC"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn topic_setting() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);

		let events = &[
			to_init_pdu_event(
				"T1",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"PA1",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"T2",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"PA2",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 0 } })).unwrap(),
			),
			to_init_pdu_event(
				"PB",
				bob(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"T3",
				bob(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"MZ1",
				zara(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"T4",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
		];

		let edges = vec![
			vec!["END", "T4", "MZ1", "PA2", "T2", "PA1", "T1", "START"],
			vec!["END", "MZ1", "T3", "PB", "PA1"],
		]
		.into_iter()
		.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
		.collect::<Vec<_>>();

		let expected_state_ids = vec!["T4", "PA2"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn test_event_map_none() {
		use futures::future::ready;

		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);

		let mut store = TestStore::<PduEvent>(hashmap! {});

		// build up the DAG
		let (state_at_bob, state_at_charlie, expected) = store.set_up();

		let ev_map = store.0.clone();
		let fetcher = |id| ready(ev_map.get(&id).cloned());

		let exists = |id: <PduEvent as Event>::Id| ready(ev_map.get(&*id).is_some());

		let state_sets = [state_at_bob, state_at_charlie];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(room_id(), map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let resolved = match super::resolve(
			&RoomVersionId::V2,
			&state_sets,
			&auth_chain,
			&fetcher,
			&exists,
			1,
		)
		.await
		{
			| Ok(state) => state,
			| Err(e) => panic!("{e}"),
		};

		assert_eq!(expected, resolved);
	}

	#[tokio::test]
	async fn test_lexicographical_sort() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);

		let graph = hashmap! {
			event_id("l") => hashset![event_id("o")],
			event_id("m") => hashset![event_id("n"), event_id("o")],
			event_id("n") => hashset![event_id("o")],
			event_id("o") => hashset![], // "o" has zero outgoing edges but 4 incoming edges
			event_id("p") => hashset![event_id("o")],
		};

		let res = super::lexicographical_topological_sort(&graph, &|_id| async {
			Ok((int!(0), MilliSecondsSinceUnixEpoch(uint!(0))))
		})
		.await
		.unwrap();

		assert_eq!(
			vec!["o", "l", "n", "m", "p"],
			res.iter()
				.map(ToString::to_string)
				.map(|s| s.replace('$', "").replace(":foo", ""))
				.collect::<Vec<_>>()
		);
	}

	#[tokio::test]
	async fn ban_with_auth_chains() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);
		let ban = BAN_STATE_SET();

		let edges = vec![vec!["END", "MB", "PA", "START"], vec!["END", "IME", "MB"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec!["PA", "MB"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(&ban.values().cloned().collect::<Vec<_>>(), edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn ban_with_auth_chains2() {
		use futures::future::ready;

		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);
		let init = INITIAL_EVENTS();
		let ban = BAN_STATE_SET();

		let mut inner = init.clone();
		inner.extend(ban);
		let store = TestStore(inner.clone());

		let state_set_a = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("MB")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| {
			(
				ev.event_type()
					.with_state_key(ev.state_key().unwrap()),
				ev.event_id.clone(),
			)
		})
		.collect::<StateMap<_>>();

		let state_set_b = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("IME")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| {
			(
				ev.event_type()
					.with_state_key(ev.state_key().unwrap()),
				ev.event_id.clone(),
			)
		})
		.collect::<StateMap<_>>();

		let ev_map = &store.0;
		let state_sets = [state_set_a, state_set_b];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(room_id(), map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let fetcher = |id: <PduEvent as Event>::Id| ready(ev_map.get(&id).cloned());
		let exists = |id: <PduEvent as Event>::Id| ready(ev_map.get(&id).is_some());
		let resolved = match super::resolve(
			&RoomVersionId::V6,
			&state_sets,
			&auth_chain,
			&fetcher,
			&exists,
			1,
		)
		.await
		{
			| Ok(state) => state,
			| Err(e) => panic!("{e}"),
		};

		debug!(
			resolved = ?resolved
				.iter()
				.map(|((ty, key), id)| format!("(({ty}{key:?}), {id})"))
				.collect::<Vec<_>>(),
				"resolved state",
		);

		let expected = [
			"$CREATE:foo",
			"$IJR:foo",
			"$PA:foo",
			"$IMA:foo",
			"$IMB:foo",
			"$IMC:foo",
			"$MB:foo",
		];

		for id in expected.iter().map(|i| event_id(i)) {
			// make sure our resolved events are equal to the expected list
			assert!(resolved.values().any(|eid| eid == &id) || init.contains_key(&id), "{id}");
		}
		assert_eq!(expected.len(), resolved.len());
	}

	#[tokio::test]
	async fn join_rule_with_auth_chain() {
		let join_rule = JOIN_RULE();

		let edges = vec![vec!["END", "JR", "START"], vec!["END", "IMZ", "START"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec!["JR"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(&join_rule.values().cloned().collect::<Vec<_>>(), edges, expected_state_ids)
			.await;
	}

	#[allow(non_snake_case)]
	fn BAN_STATE_SET() -> HashMap<OwnedEventId, PduEvent> {
		vec![
			to_pdu_event(
				"PA",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
				&["CREATE", "IMA", "IPOWER"], // auth_events
				&["START"],                   // prev_events
			),
			to_pdu_event(
				"PB",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
				&["CREATE", "IMA", "IPOWER"],
				&["END"],
			),
			to_pdu_event(
				"MB",
				alice(),
				TimelineEventType::RoomMember,
				Some(ella().as_str()),
				member_content_ban(),
				&["CREATE", "IMA", "PB"],
				&["PA"],
			),
			to_pdu_event(
				"IME",
				ella(),
				TimelineEventType::RoomMember,
				Some(ella().as_str()),
				member_content_join(),
				&["CREATE", "IJR", "PA"],
				&["MB"],
			),
		]
		.into_iter()
		.map(|ev| (ev.event_id.clone(), ev))
		.collect()
	}

	#[allow(non_snake_case)]
	fn JOIN_RULE() -> HashMap<OwnedEventId, PduEvent> {
		vec![
			to_pdu_event(
				"JR",
				alice(),
				TimelineEventType::RoomJoinRules,
				Some(""),
				to_raw_json_value(&json!({ "join_rule": "invite" })).unwrap(),
				&["CREATE", "IMA", "IPOWER"],
				&["START"],
			),
			to_pdu_event(
				"IMZ",
				zara(),
				TimelineEventType::RoomPowerLevels,
				Some(zara().as_str()),
				member_content_join(),
				&["CREATE", "JR", "IPOWER"],
				&["START"],
			),
		]
		.into_iter()
		.map(|ev| (ev.event_id.clone(), ev))
		.collect()
	}

	macro_rules! state_set {
        ($($kind:expr_2021 => $key:expr_2021 => $id:expr_2021),* $(,)?) => {{
            #[allow(unused_mut)]
            let mut x = StateMap::new();
            $(
                x.insert(($kind, $key.into()), $id);
            )*
            x
        }};
    }

	#[test]
	fn separate_unique_conflicted() {
		let (unconflicted, conflicted) = super::separate(
			[
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
				state_set![StateEventType::RoomMember => "@b:hs1" => 1],
				state_set![StateEventType::RoomMember => "@c:hs1" => 2],
			]
			.iter(),
		);

		assert_eq!(unconflicted, StateMap::new());
		assert_eq!(
			conflicted,
			state_set![
				StateEventType::RoomMember => "@a:hs1" => vec![0],
				StateEventType::RoomMember => "@b:hs1" => vec![1],
				StateEventType::RoomMember => "@c:hs1" => vec![2],
			],
		);
	}

	#[test]
	fn separate_conflicted() {
		let (unconflicted, mut conflicted) = super::separate(
			[
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
				state_set![StateEventType::RoomMember => "@a:hs1" => 1],
				state_set![StateEventType::RoomMember => "@a:hs1" => 2],
			]
			.iter(),
		);

		// HashMap iteration order is random, so sort this before asserting on it
		for v in conflicted.values_mut() {
			v.sort_unstable();
		}

		assert_eq!(unconflicted, StateMap::new());
		assert_eq!(
			conflicted,
			state_set![
				StateEventType::RoomMember => "@a:hs1" => vec![0, 1, 2],
			],
		);
	}

	#[test]
	fn separate_unconflicted() {
		let (unconflicted, conflicted) = super::separate(
			[
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
			]
			.iter(),
		);

		assert_eq!(
			unconflicted,
			state_set![
				StateEventType::RoomMember => "@a:hs1" => 0,
			],
		);
		assert_eq!(conflicted, StateMap::new());
	}

	#[test]
	fn separate_mixed() {
		let (unconflicted, conflicted) = super::separate(
			[
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
				state_set![
					StateEventType::RoomMember => "@a:hs1" => 0,
					StateEventType::RoomMember => "@b:hs1" => 1,
				],
				state_set![
					StateEventType::RoomMember => "@a:hs1" => 0,
					StateEventType::RoomMember => "@c:hs1" => 2,
				],
			]
			.iter(),
		);

		assert_eq!(
			unconflicted,
			state_set![
				StateEventType::RoomMember => "@a:hs1" => 0,
			],
		);
		assert_eq!(
			conflicted,
			state_set![
				StateEventType::RoomMember => "@b:hs1" => vec![1],
				StateEventType::RoomMember => "@c:hs1" => vec![2],
			],
		);
	}
}
