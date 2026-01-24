use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	pin::Pin,
	slice,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering::SeqCst},
	},
};

use ruma::{
	EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, UserId, event_id,
	events::{
		StateEventType, TimelineEventType,
		room::{
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
		},
	},
	int, room_id,
	room_version_rules::{AuthorizationRules, RoomVersionRules},
	uint, user_id,
};
use serde_json::{
	json,
	value::{RawValue as RawJsonValue, to_raw_value as to_raw_json_value},
};

use super::{AuthSet, StateMap, auth_types_for_event, events::RoomCreateEvent};
use crate::{
	Error, Result, err, info,
	matrix::{Event, EventHash, EventTypeExt, PduEvent, StateKey},
	utils::stream::IterStream,
};

static SERVER_TIMESTAMP: AtomicU64 = AtomicU64::new(0);

pub(super) fn not_found() -> Error { err!(Request(NotFound("Test event not found"))) }

pub(super) fn event_not_found(event_id: &EventId) -> Error {
	err!(Request(NotFound("Test event not found: {event_id:?}")))
}

pub(super) fn state_not_found(ty: &StateEventType, sk: &str) -> Error {
	err!(Request(NotFound("Test state not found: ({ty:?},{sk:?})")))
}

pub(super) async fn do_check(
	events: &[PduEvent],
	edges: Vec<Vec<OwnedEventId>>,
	expected_state_ids: Vec<OwnedEventId>,
) {
	// To activate logging use `RUST_LOG=debug cargo t`

	let init_events = INITIAL_EVENTS();

	let mut store = TestStore(
		init_events
			.values()
			.chain(events)
			.map(|ev| (ev.event_id().to_owned(), ev.clone()))
			.collect(),
	);

	// This will be lexi_topo_sorted for resolution
	let mut graph = HashMap::new();
	// This is the same as in `resolve` event_id -> OriginalStateEvent
	let mut fake_event_map = HashMap::new();

	// Create the DB of events that led up to this point
	// TODO maybe clean up some of these clones it is just tests but...
	for ev in init_events.values().chain(events) {
		graph.insert(ev.event_id().to_owned(), HashSet::new());
		fake_event_map.insert(ev.event_id().to_owned(), ev.clone());
	}

	for pair in INITIAL_EDGES().windows(2) {
		if let [a, b] = &pair {
			graph
				.entry(a.to_owned())
				.or_insert_with(HashSet::new)
				.insert(b.clone());
		}
	}

	for edge_list in edges {
		for pair in edge_list.windows(2) {
			if let [a, b] = &pair {
				graph
					.entry(a.to_owned())
					.or_insert_with(HashSet::new)
					.insert(b.clone());
			}
		}
	}

	// event_id -> PduEvent
	let mut event_map: HashMap<OwnedEventId, PduEvent> = HashMap::new();
	// event_id -> StateMap<OwnedEventId>
	let mut state_at_event: HashMap<OwnedEventId, StateMap<OwnedEventId>> = HashMap::new();

	// Resolve the current state and add it to the state_at_event map then continue
	// on in "time"
	for node in super::topological_sort(&graph, &async |_id| {
		Ok((int!(0).into(), MilliSecondsSinceUnixEpoch(uint!(0))))
	})
	.await
	.unwrap()
	{
		let fake_event = &fake_event_map[&node];
		let event_id = fake_event.event_id().to_owned();

		let prev_events = &graph[&node];

		let state_before: StateMap<OwnedEventId> = if prev_events.is_empty() {
			StateMap::new()
		} else if prev_events.len() == 1 {
			state_at_event[prev_events.iter().next().unwrap()].clone()
		} else {
			let state_sets = prev_events
				.iter()
				.filter_map(|k| state_at_event.get(k).cloned())
				.collect::<Vec<_>>();

			info!(
				"{:#?}",
				state_sets
					.iter()
					.map(|map| map
						.iter()
						.map(|((ty, key), id)| format!("(({ty}{key:?}), {id})"))
						.collect::<Vec<_>>())
					.collect::<Vec<_>>()
			);

			let auth_chain_sets = state_sets
				.iter()
				.map(|map| {
					store
						.auth_event_ids(room_id(), map.values().cloned().collect())
						.unwrap()
				})
				.collect::<Vec<_>>();

			let state_sets = state_sets.into_iter().stream();

			let rules = RoomVersionRules::V6;
			let resolved = super::resolve(
				&rules,
				state_sets,
				auth_chain_sets.into_iter().stream(),
				&async |id| event_map.get(&id).cloned().ok_or_else(not_found),
				&async |id| event_map.contains_key(&id),
				false,
			)
			.await;

			match resolved {
				| Ok(state) => state,
				| Err(e) => panic!("resolution for {node} failed: {e}"),
			}
		};

		let mut state_after = state_before.clone();

		let ty = fake_event.event_type();
		let key = fake_event.state_key().unwrap();
		state_after.insert(ty.with_state_key(key), event_id.clone());

		let auth_types = auth_types_for_event(
			fake_event.event_type(),
			fake_event.sender(),
			fake_event.state_key(),
			fake_event.content(),
			&AuthorizationRules::V6,
			false,
		)
		.unwrap();

		let mut auth_events = vec![];
		for key in auth_types {
			if state_before.contains_key(&key) {
				auth_events.push(state_before[&key].clone());
			}
		}

		// TODO The event is just remade, adding the auth_events and prev_events here
		// the `to_pdu_event` was split into `init` and the fn below, could be better
		let e = fake_event;
		let ev_id = e.event_id();
		let event = to_pdu_event(
			e.event_id().as_str(),
			e.sender(),
			e.event_type().clone(),
			e.state_key(),
			e.content().to_owned(),
			&auth_events,
			&prev_events.iter().cloned().collect::<Vec<_>>(),
		);

		// We have to update our store, an actual user of this lib would
		// be giving us state from a DB.
		store.0.insert(ev_id.to_owned(), event.clone());

		state_at_event.insert(node, state_after);
		event_map.insert(event_id.clone(), store.0[ev_id].clone());
	}

	let mut expected_state = StateMap::new();
	for node in expected_state_ids {
		let ev = event_map.get(&node).unwrap_or_else(|| {
			panic!(
				"{node} not found in {:?}",
				event_map
					.keys()
					.map(ToString::to_string)
					.collect::<Vec<_>>()
			)
		});

		let key = ev
			.event_type()
			.with_state_key(ev.state_key().unwrap());

		expected_state.insert(key, node);
	}

	let start_state = state_at_event
		.get(event_id!("$START:foo"))
		.unwrap();

	let end_state = state_at_event
		.get(event_id!("$END:foo"))
		.unwrap()
		.iter()
		.filter(|(k, v)| {
			expected_state.contains_key(k)
				|| start_state.get(k) != Some(*v)
                // Filter out the dummy messages events.
                // These act as points in time where there should be a known state to
                // test against.
                && **k != ("m.room.message".into(), "dummy".into())
		})
		.map(|(k, v)| (k.clone(), v.clone()))
		.collect::<StateMap<OwnedEventId>>();

	assert_eq!(expected_state, end_state);
}

pub(super) struct TestStore(pub(super) HashMap<OwnedEventId, PduEvent>);

impl TestStore {
	pub(super) fn get_event(&self, _: &RoomId, event_id: &EventId) -> Result<PduEvent> {
		self.0
			.get(event_id)
			.cloned()
			.ok_or_else(|| event_not_found(event_id))
	}

	/// Returns a Vec of the related auth events to the given `event`.
	pub(super) fn auth_event_ids(
		&self,
		room_id: &RoomId,
		event_ids: Vec<OwnedEventId>,
	) -> Result<AuthSet<OwnedEventId>> {
		let mut result = AuthSet::new();
		let mut stack = event_ids;

		// DFS for auth event chain
		while let Some(ev_id) = stack.pop() {
			if result.contains(&ev_id) {
				continue;
			}

			result.insert(ev_id.clone());

			let event = self.get_event(room_id, ev_id.borrow())?;

			stack.extend(event.auth_events().map(ToOwned::to_owned));
		}

		Ok(result)
	}
}

// A StateStore implementation for testing
impl TestStore {
	pub(super) fn set_up(
		&mut self,
	) -> (StateMap<OwnedEventId>, StateMap<OwnedEventId>, StateMap<OwnedEventId>) {
		let create_event = to_pdu_event::<&EventId>(
			"CREATE",
			alice(),
			TimelineEventType::RoomCreate,
			Some(""),
			to_raw_json_value(&json!({ "creator": alice() })).unwrap(),
			&[],
			&[],
		);
		let cre = create_event.event_id().to_owned();
		self.0.insert(cre.clone(), create_event.clone());

		let alice_mem = to_pdu_event(
			"IMA",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			slice::from_ref(&cre),
			slice::from_ref(&cre),
		);
		self.0
			.insert(alice_mem.event_id().to_owned(), alice_mem.clone());

		let join_rules = to_pdu_event(
			"IJR",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Public)).unwrap(),
			&[cre.clone(), alice_mem.event_id().to_owned()],
			&[alice_mem.event_id().to_owned()],
		);
		self.0
			.insert(join_rules.event_id().to_owned(), join_rules.clone());

		// Bob and Charlie join at the same time, so there is a fork
		// this will be represented in the state_sets when we resolve
		let bob_mem = to_pdu_event(
			"IMB",
			bob(),
			TimelineEventType::RoomMember,
			Some(bob().as_str()),
			member_content_join(),
			&[cre.clone(), join_rules.event_id().to_owned()],
			&[join_rules.event_id().to_owned()],
		);
		self.0
			.insert(bob_mem.event_id().to_owned(), bob_mem.clone());

		let charlie_mem = to_pdu_event(
			"IMC",
			charlie(),
			TimelineEventType::RoomMember,
			Some(charlie().as_str()),
			member_content_join(),
			&[cre, join_rules.event_id().to_owned()],
			&[join_rules.event_id().to_owned()],
		);
		self.0
			.insert(charlie_mem.event_id().to_owned(), charlie_mem.clone());

		let state_at_bob = [&create_event, &alice_mem, &join_rules, &bob_mem]
			.iter()
			.map(|e| {
				(
					e.event_type()
						.with_state_key(e.state_key().unwrap()),
					e.event_id().to_owned(),
				)
			})
			.collect::<StateMap<_>>();

		let state_at_charlie = [&create_event, &alice_mem, &join_rules, &charlie_mem]
			.iter()
			.map(|e| {
				(
					e.event_type()
						.with_state_key(e.state_key().unwrap()),
					e.event_id().to_owned(),
				)
			})
			.collect::<StateMap<_>>();

		let expected = [&create_event, &alice_mem, &join_rules, &bob_mem, &charlie_mem]
			.iter()
			.map(|e| {
				(
					e.event_type()
						.with_state_key(e.state_key().unwrap()),
					e.event_id().to_owned(),
				)
			})
			.collect::<StateMap<_>>();

		(state_at_bob, state_at_charlie, expected)
	}
}

pub(super) fn event_id(id: &str) -> OwnedEventId {
	if id.contains('$') {
		return id.try_into().unwrap();
	}

	format!("${id}:foo").try_into().unwrap()
}

pub(super) fn alice() -> &'static UserId { user_id!("@alice:foo") }

pub(super) fn bob() -> &'static UserId { user_id!("@bob:foo") }

pub(super) fn charlie() -> &'static UserId { user_id!("@charlie:foo") }

pub(super) fn ella() -> &'static UserId { user_id!("@ella:foo") }

pub(super) fn zara() -> &'static UserId { user_id!("@zara:foo") }

pub(super) fn room_id() -> &'static RoomId { room_id!("!test:foo") }

pub(crate) fn hydra_room_id() -> &'static RoomId { room_id!("!CREATE") }

pub(super) fn member_content_ban() -> Box<RawJsonValue> {
	to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Ban)).unwrap()
}

pub(super) fn member_content_join() -> Box<RawJsonValue> {
	to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Join)).unwrap()
}

pub(super) fn to_init_pdu_event(
	id: &str,
	sender: &UserId,
	ev_type: TimelineEventType,
	state_key: Option<&str>,
	content: Box<RawJsonValue>,
) -> PduEvent {
	let ts = SERVER_TIMESTAMP.fetch_add(1, SeqCst);
	let state_key = state_key.map(ToOwned::to_owned);
	let id = if id.contains('$') {
		id.to_owned()
	} else {
		format!("${id}:foo")
	};

	PduEvent {
		event_id: id.try_into().unwrap(),
		room_id: room_id().to_owned(),
		sender: sender.to_owned(),
		origin: None,
		origin_server_ts: ts.try_into().unwrap(),
		state_key: state_key.map(Into::into),
		kind: ev_type,
		content,
		redacts: None,
		unsigned: None,
		auth_events: Default::default(),
		prev_events: Default::default(),
		depth: uint!(0),
		hashes: EventHash::default(),
		signatures: None,
		rejected: false,
	}
}

pub(super) fn to_pdu_event<S>(
	id: &str,
	sender: &UserId,
	ev_type: TimelineEventType,
	state_key: Option<&str>,
	content: Box<RawJsonValue>,
	auth_events: &[S],
	prev_events: &[S],
) -> PduEvent
where
	S: AsRef<str>,
{
	let ts = SERVER_TIMESTAMP.fetch_add(1, SeqCst);
	let state_key = state_key.map(ToOwned::to_owned);
	let id = if id.contains('$') {
		id.to_owned()
	} else {
		format!("${id}:foo")
	};
	let auth_events = auth_events
		.iter()
		.map(AsRef::as_ref)
		.map(event_id)
		.collect();
	let prev_events = prev_events
		.iter()
		.map(AsRef::as_ref)
		.map(event_id)
		.collect();

	PduEvent {
		event_id: id.try_into().unwrap(),
		room_id: room_id().to_owned(),
		sender: sender.to_owned(),
		origin: None,
		origin_server_ts: ts.try_into().unwrap(),
		state_key: state_key.map(Into::into),
		kind: ev_type,
		content,
		redacts: None,
		unsigned: None,
		auth_events,
		prev_events,
		depth: uint!(0),
		hashes: EventHash::default(),
		signatures: None,
		rejected: false,
	}
}

/// Same as `to_pdu_event()`, but uses the default m.room.create event ID to
/// generate the room ID.
pub(super) fn to_hydra_pdu_event<S>(
	id: &str,
	sender: &UserId,
	ev_type: TimelineEventType,
	state_key: Option<&str>,
	content: Box<RawJsonValue>,
	auth_events: &[S],
	prev_events: &[S],
) -> PduEvent
where
	S: AsRef<str>,
{
	fn event_id(id: &str) -> OwnedEventId {
		if id.contains('$') {
			id.try_into().unwrap()
		} else {
			format!("${id}").try_into().unwrap()
		}
	}

	let ts = SERVER_TIMESTAMP.fetch_add(1, SeqCst);
	let state_key = state_key.map(ToOwned::to_owned);
	let auth_events = auth_events
		.iter()
		.map(AsRef::as_ref)
		.map(event_id)
		.collect();
	let prev_events = prev_events
		.iter()
		.map(AsRef::as_ref)
		.map(event_id)
		.collect();

	PduEvent {
		event_id: event_id(id),
		room_id: hydra_room_id().to_owned(),
		sender: sender.to_owned(),
		origin: None,
		origin_server_ts: ts.try_into().unwrap(),
		state_key: state_key.map(Into::into),
		kind: ev_type,
		content,
		redacts: None,
		unsigned: None,
		auth_events,
		prev_events,
		depth: uint!(0),
		hashes: EventHash::default(),
		signatures: None,
		rejected: false,
	}
}

pub(super) fn room_redaction_pdu_event<S>(
	id: &str,
	sender: &UserId,
	redacts: OwnedEventId,
	content: Box<RawJsonValue>,
	auth_events: &[S],
	prev_events: &[S],
) -> PduEvent
where
	S: AsRef<str>,
{
	let ts = SERVER_TIMESTAMP.fetch_add(1, SeqCst);
	let id = if id.contains('$') {
		id.to_owned()
	} else {
		format!("${id}:foo")
	};
	let auth_events = auth_events
		.iter()
		.map(AsRef::as_ref)
		.map(event_id)
		.collect();
	let prev_events = prev_events
		.iter()
		.map(AsRef::as_ref)
		.map(event_id)
		.collect();

	PduEvent {
		event_id: id.try_into().unwrap(),
		room_id: room_id().to_owned(),
		sender: sender.to_owned(),
		origin: None,
		origin_server_ts: ts.try_into().unwrap(),
		state_key: None,
		kind: TimelineEventType::RoomRedaction,
		content,
		redacts: Some(redacts),
		unsigned: None,
		auth_events,
		prev_events,
		depth: uint!(0),
		hashes: EventHash::default(),
		signatures: None,
		rejected: false,
	}
}

pub(super) fn room_create_hydra_pdu_event(
	id: &str,
	sender: &UserId,
	content: Box<RawJsonValue>,
) -> PduEvent {
	let ts = SERVER_TIMESTAMP.fetch_add(1, SeqCst);
	let eid = if id.contains('$') {
		id.to_owned()
	} else {
		format!("${id}")
	};
	let rid = if id.contains('!') {
		id.to_owned()
	} else {
		format!("!{id}")
	};

	PduEvent {
		event_id: eid.try_into().unwrap(),
		room_id: rid.try_into().unwrap(),
		sender: sender.to_owned(),
		origin: None,
		origin_server_ts: ts.try_into().unwrap(),
		state_key: Some(StateKey::new()),
		kind: TimelineEventType::RoomCreate,
		content,
		redacts: None,
		unsigned: None,
		auth_events: Default::default(),
		prev_events: Default::default(),
		depth: uint!(0),
		hashes: EventHash::default(),
		signatures: None,
		rejected: false,
	}
}

// all graphs start with these input events
#[expect(non_snake_case)]
pub(super) fn INITIAL_EVENTS() -> HashMap<OwnedEventId, PduEvent> {
	vec![
		to_pdu_event::<&EventId>(
			"CREATE",
			alice(),
			TimelineEventType::RoomCreate,
			Some(""),
			to_raw_json_value(&json!({ "creator": alice() })).unwrap(),
			&[],
			&[],
		),
		to_pdu_event(
			"IMA",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			&["CREATE"],
			&["CREATE"],
		),
		to_pdu_event(
			"IPOWER",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({ "users": { alice(): 100 } })).unwrap(),
			&["CREATE", "IMA"],
			&["IMA"],
		),
		to_pdu_event(
			"IJR",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Public)).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		),
		to_pdu_event(
			"IMB",
			bob(),
			TimelineEventType::RoomMember,
			Some(bob().as_str()),
			member_content_join(),
			&["CREATE", "IJR", "IPOWER"],
			&["IJR"],
		),
		to_pdu_event(
			"IMC",
			charlie(),
			TimelineEventType::RoomMember,
			Some(charlie().as_str()),
			member_content_join(),
			&["CREATE", "IJR", "IPOWER"],
			&["IMB"],
		),
		to_pdu_event::<&EventId>(
			"START",
			charlie(),
			TimelineEventType::RoomMessage,
			Some("dummy"),
			to_raw_json_value(&json!({})).unwrap(),
			&[],
			&[],
		),
		to_pdu_event::<&EventId>(
			"END",
			charlie(),
			TimelineEventType::RoomMessage,
			Some("dummy"),
			to_raw_json_value(&json!({})).unwrap(),
			&[],
			&[],
		),
	]
	.into_iter()
	.map(|ev| (ev.event_id().to_owned(), ev))
	.collect()
}

/// Batch of initial events to use for incoming events from room version
/// `org.matrix.hydra.11` onwards.
#[expect(non_snake_case)]
pub(super) fn INITIAL_HYDRA_EVENTS() -> HashMap<OwnedEventId, PduEvent> {
	vec![
		room_create_hydra_pdu_event(
			"CREATE",
			alice(),
			to_raw_json_value(&json!({ "room_version": "org.matrix.hydra.11" })).unwrap(),
		),
		to_hydra_pdu_event(
			"IMA",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			&["CREATE"],
			&["CREATE"],
		),
		to_hydra_pdu_event(
			"IPOWER",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({})).unwrap(),
			&["CREATE", "IMA"],
			&["IMA"],
		),
		to_hydra_pdu_event(
			"IJR",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Public)).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		),
		to_hydra_pdu_event(
			"IMB",
			bob(),
			TimelineEventType::RoomMember,
			Some(bob().as_str()),
			member_content_join(),
			&["CREATE", "IJR", "IPOWER"],
			&["IJR"],
		),
		to_hydra_pdu_event(
			"IMC",
			charlie(),
			TimelineEventType::RoomMember,
			Some(charlie().as_str()),
			member_content_join(),
			&["CREATE", "IJR", "IPOWER"],
			&["IMB"],
		),
		to_hydra_pdu_event::<&EventId>(
			"START",
			charlie(),
			TimelineEventType::RoomMessage,
			Some("dummy"),
			to_raw_json_value(&json!({})).unwrap(),
			&[],
			&[],
		),
		to_hydra_pdu_event::<&EventId>(
			"END",
			charlie(),
			TimelineEventType::RoomMessage,
			Some("dummy"),
			to_raw_json_value(&json!({})).unwrap(),
			&[],
			&[],
		),
	]
	.into_iter()
	.map(|ev| (ev.event_id().to_owned(), ev))
	.collect()
}

// all graphs start with these input events
#[expect(non_snake_case)]
pub(super) fn INITIAL_EVENTS_CREATE_ROOM() -> HashMap<OwnedEventId, PduEvent> {
	vec![to_pdu_event::<&EventId>(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&json!({ "creator": alice() })).unwrap(),
		&[],
		&[],
	)]
	.into_iter()
	.map(|ev| (ev.event_id().to_owned(), ev))
	.collect()
}

#[expect(non_snake_case)]
pub(super) fn INITIAL_EDGES() -> Vec<OwnedEventId> {
	vec!["START", "IMC", "IMB", "IJR", "IPOWER", "IMA", "CREATE"]
		.into_iter()
		.map(event_id)
		.collect::<Vec<_>>()
}

pub(super) fn init_subscriber() -> tracing::dispatcher::DefaultGuard {
	tracing::subscriber::set_default(
		tracing_subscriber::fmt()
			.with_test_writer()
			.finish(),
	)
}

/// Wrapper around a state map.
pub(super) struct TestStateMap(HashMap<StateEventType, HashMap<String, PduEvent>>);

impl TestStateMap {
	/// Construct a `TestStateMap` from the given event map.
	pub(super) fn new(events: &HashMap<OwnedEventId, PduEvent>) -> Arc<Self> {
		let mut state_map: HashMap<StateEventType, HashMap<String, PduEvent>> = HashMap::new();

		for event in events.values() {
			let event_type = StateEventType::from(event.event_type().to_string());

			state_map
				.entry(event_type)
				.or_default()
				.insert(event.state_key().unwrap().to_owned(), event.clone());
		}

		Arc::new(Self(state_map))
	}

	/// Get the event with the given event type and state key.
	pub(super) fn get(
		self: &Arc<Self>,
		event_type: &StateEventType,
		state_key: &str,
	) -> Result<PduEvent> {
		self.0
			.get(event_type)
			.ok_or_else(|| state_not_found(event_type, state_key))?
			.get(state_key)
			.cloned()
			.ok_or_else(|| state_not_found(event_type, state_key))
	}

	/// A function to get a state event from this map.
	pub(super) fn fetch_state_fn(
		self: &Arc<Self>,
	) -> impl Fn(StateEventType, StateKey) -> Pin<Box<dyn Future<Output = Result<PduEvent>> + Send>>
	{
		move |event_type: StateEventType, state_key: StateKey| {
			let s = self.clone();
			Box::pin(async move { s.get(&event_type, state_key.as_str()) })
		}
	}

	/// The `m.room.create` event contained in this map.
	///
	/// Panics if there is no `m.room.create` event in this map.
	pub(super) fn room_create_event(self: &Arc<Self>) -> RoomCreateEvent<PduEvent> {
		RoomCreateEvent::new(self.get(&StateEventType::RoomCreate, "").unwrap())
	}
}

/// Create an `m.room.third_party_invite` event with the given sender.
pub(super) fn room_third_party_invite(sender: &UserId) -> PduEvent {
	let content = json!({
		"display_name": "o...@g...",
		"key_validity_url": "https://identity.local/_matrix/identity/v2/pubkey/isvalid",
		"public_key": "Gb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE",
		"public_keys": [
			{
				"key_validity_url": "https://identity.local/_matrix/identity/v2/pubkey/isvalid",
				"public_key": "Gb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE"
			},
			{
				"key_validity_url": "https://identity.local/_matrix/identity/v2/pubkey/ephemeral/isvalid",
				"public_key": "Kxdvv7lo0O6JVI7yimFgmYPfpLGnctcpYjuypP5zx/c"
			}
		]
	});

	to_pdu_event(
		"THIRDPARTY",
		sender,
		TimelineEventType::RoomThirdPartyInvite,
		Some("somerandomtoken"),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	)
}
