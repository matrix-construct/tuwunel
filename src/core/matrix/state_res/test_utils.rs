use std::{
	borrow::Borrow,
	collections::{BTreeMap, HashMap, HashSet},
	sync::atomic::{AtomicU64, Ordering::SeqCst},
};

use futures::future::ready;
use ruma::{
	EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, RoomVersionId, ServerSignatures,
	UserId, event_id,
	events::{
		TimelineEventType,
		pdu::{EventHash, Pdu, RoomV3Pdu},
		room::{
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
		},
	},
	int, room_id, uint, user_id,
};
use serde_json::{
	json,
	value::{RawValue as RawJsonValue, to_raw_value as to_raw_json_value},
};

pub(crate) use self::event::PduEvent;
use super::auth_types_for_event;
use crate::{
	Result, info,
	matrix::{Event, EventTypeExt, StateMap},
};

static SERVER_TIMESTAMP: AtomicU64 = AtomicU64::new(0);

pub(crate) async fn do_check(
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
	for node in super::lexicographical_topological_sort(&graph, &|_id| async {
		Ok((int!(0), MilliSecondsSinceUnixEpoch(uint!(0))))
	})
	.await
	.unwrap()
	{
		let fake_event = fake_event_map.get(&node).unwrap();
		let event_id = fake_event.event_id().to_owned();

		let prev_events = graph.get(&node).unwrap();

		let state_before: StateMap<OwnedEventId> = if prev_events.is_empty() {
			HashMap::new()
		} else if prev_events.len() == 1 {
			state_at_event
				.get(prev_events.iter().next().unwrap())
				.unwrap()
				.clone()
		} else {
			let state_sets = prev_events
				.iter()
				.filter_map(|k| state_at_event.get(k))
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

			let auth_chain_sets: Vec<_> = state_sets
				.iter()
				.map(|map| {
					store
						.auth_event_ids(room_id(), map.values().cloned().collect())
						.unwrap()
				})
				.collect();

			let event_map = &event_map;
			let fetch = |id: <PduEvent as Event>::Id| ready(event_map.get(&id).cloned());
			let exists = |id: <PduEvent as Event>::Id| ready(event_map.get(&id).is_some());
			let resolved = super::resolve(
				&RoomVersionId::V6,
				state_sets,
				&auth_chain_sets,
				&fetch,
				&exists,
				1,
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
		state_after.insert(ty.with_state_key(key), event_id.to_owned());

		let auth_types = auth_types_for_event(
			fake_event.event_type(),
			fake_event.sender(),
			fake_event.state_key(),
			fake_event.content(),
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
		event_map.insert(event_id.to_owned(), store.0.get(ev_id).unwrap().clone());
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

#[allow(clippy::exhaustive_structs)]
pub(crate) struct TestStore<E: Event>(pub(crate) HashMap<OwnedEventId, E>);

impl<E: Event + Clone> TestStore<E> {
	pub(crate) fn get_event(&self, _: &RoomId, event_id: &EventId) -> Result<E> {
		self.0
			.get(event_id)
			.cloned()
			.ok_or_else(|| super::Error::NotFound(format!("{event_id} not found")))
			.map_err(Into::into)
	}

	/// Returns a Vec of the related auth events to the given `event`.
	pub(crate) fn auth_event_ids(
		&self,
		room_id: &RoomId,
		event_ids: Vec<E::Id>,
	) -> Result<HashSet<E::Id>> {
		let mut result = HashSet::new();
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
#[allow(clippy::type_complexity)]
impl TestStore<PduEvent> {
	pub(crate) fn set_up(
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
			&[cre.clone()],
			&[cre.clone()],
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

pub(crate) fn event_id(id: &str) -> OwnedEventId {
	if id.contains('$') {
		return id.try_into().unwrap();
	}

	format!("${id}:foo").try_into().unwrap()
}

pub(crate) fn alice() -> &'static UserId {
	user_id!("@alice:foo")
}

pub(crate) fn bob() -> &'static UserId {
	user_id!("@bob:foo")
}

pub(crate) fn charlie() -> &'static UserId {
	user_id!("@charlie:foo")
}

pub(crate) fn ella() -> &'static UserId {
	user_id!("@ella:foo")
}

pub(crate) fn zara() -> &'static UserId {
	user_id!("@zara:foo")
}

pub(crate) fn room_id() -> &'static RoomId {
	room_id!("!test:foo")
}

pub(crate) fn member_content_ban() -> Box<RawJsonValue> {
	to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Ban)).unwrap()
}

pub(crate) fn member_content_join() -> Box<RawJsonValue> {
	to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Join)).unwrap()
}

pub(crate) fn to_init_pdu_event(
	id: &str,
	sender: &UserId,
	ev_type: TimelineEventType,
	state_key: Option<&str>,
	content: Box<RawJsonValue>,
) -> PduEvent {
	let ts = SERVER_TIMESTAMP.fetch_add(1, SeqCst);
	let id = if id.contains('$') {
		id.to_owned()
	} else {
		format!("${id}:foo")
	};

	let state_key = state_key.map(ToOwned::to_owned);
	PduEvent {
		event_id: id.try_into().unwrap(),
		rest: Pdu::RoomV3Pdu(RoomV3Pdu {
			room_id: room_id().to_owned(),
			sender: sender.to_owned(),
			origin_server_ts: MilliSecondsSinceUnixEpoch(ts.try_into().unwrap()),
			state_key,
			kind: ev_type,
			content,
			redacts: None,
			unsigned: BTreeMap::new(),
			auth_events: vec![],
			prev_events: vec![],
			depth: uint!(0),
			hashes: EventHash::new("".to_owned()),
			signatures: ServerSignatures::default(),
		}),
	}
}

pub(crate) fn to_pdu_event<S>(
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
	let id = if id.contains('$') {
		id.to_owned()
	} else {
		format!("${id}:foo")
	};
	let auth_events = auth_events
		.iter()
		.map(AsRef::as_ref)
		.map(event_id)
		.collect::<Vec<_>>();
	let prev_events = prev_events
		.iter()
		.map(AsRef::as_ref)
		.map(event_id)
		.collect::<Vec<_>>();

	let state_key = state_key.map(ToOwned::to_owned);
	PduEvent {
		event_id: id.try_into().unwrap(),
		rest: Pdu::RoomV3Pdu(RoomV3Pdu {
			room_id: room_id().to_owned(),
			sender: sender.to_owned(),
			origin_server_ts: MilliSecondsSinceUnixEpoch(ts.try_into().unwrap()),
			state_key,
			kind: ev_type,
			content,
			redacts: None,
			unsigned: BTreeMap::new(),
			auth_events,
			prev_events,
			depth: uint!(0),
			hashes: EventHash::new("".to_owned()),
			signatures: ServerSignatures::default(),
		}),
	}
}

// all graphs start with these input events
#[allow(non_snake_case)]
pub(crate) fn INITIAL_EVENTS() -> HashMap<OwnedEventId, PduEvent> {
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

// all graphs start with these input events
#[allow(non_snake_case)]
pub(crate) fn INITIAL_EVENTS_CREATE_ROOM() -> HashMap<OwnedEventId, PduEvent> {
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

#[allow(non_snake_case)]
pub(crate) fn INITIAL_EDGES() -> Vec<OwnedEventId> {
	vec!["START", "IMC", "IMB", "IJR", "IPOWER", "IMA", "CREATE"]
		.into_iter()
		.map(event_id)
		.collect::<Vec<_>>()
}

pub(crate) mod event {
	use ruma::{
		MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, UserId,
		events::{TimelineEventType, pdu::Pdu},
	};
	use serde::{Deserialize, Serialize};
	use serde_json::value::RawValue as RawJsonValue;

	use crate::Event;

	impl Event for PduEvent {
		type Id = OwnedEventId;

		fn event_id(&self) -> &Self::Id {
			&self.event_id
		}

		fn room_id(&self) -> &RoomId {
			match &self.rest {
				| Pdu::RoomV1Pdu(ev) => &ev.room_id,
				| Pdu::RoomV3Pdu(ev) => &ev.room_id,
				#[allow(unreachable_patterns)]
				| _ => unreachable!("new PDU version"),
			}
		}

		fn sender(&self) -> &UserId {
			match &self.rest {
				| Pdu::RoomV1Pdu(ev) => &ev.sender,
				| Pdu::RoomV3Pdu(ev) => &ev.sender,
				#[allow(unreachable_patterns)]
				| _ => unreachable!("new PDU version"),
			}
		}

		fn event_type(&self) -> &TimelineEventType {
			match &self.rest {
				| Pdu::RoomV1Pdu(ev) => &ev.kind,
				| Pdu::RoomV3Pdu(ev) => &ev.kind,
				#[allow(unreachable_patterns)]
				| _ => unreachable!("new PDU version"),
			}
		}

		fn content(&self) -> &RawJsonValue {
			match &self.rest {
				| Pdu::RoomV1Pdu(ev) => &ev.content,
				| Pdu::RoomV3Pdu(ev) => &ev.content,
				#[allow(unreachable_patterns)]
				| _ => unreachable!("new PDU version"),
			}
		}

		fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
			match &self.rest {
				| Pdu::RoomV1Pdu(ev) => ev.origin_server_ts,
				| Pdu::RoomV3Pdu(ev) => ev.origin_server_ts,
				#[allow(unreachable_patterns)]
				| _ => unreachable!("new PDU version"),
			}
		}

		fn state_key(&self) -> Option<&str> {
			match &self.rest {
				| Pdu::RoomV1Pdu(ev) => ev.state_key.as_deref(),
				| Pdu::RoomV3Pdu(ev) => ev.state_key.as_deref(),
				#[allow(unreachable_patterns)]
				| _ => unreachable!("new PDU version"),
			}
		}

		#[allow(refining_impl_trait)]
		fn prev_events(&self) -> Box<dyn DoubleEndedIterator<Item = &Self::Id> + Send + '_> {
			match &self.rest {
				| Pdu::RoomV1Pdu(ev) => Box::new(ev.prev_events.iter().map(|(id, _)| id)),
				| Pdu::RoomV3Pdu(ev) => Box::new(ev.prev_events.iter()),
				#[allow(unreachable_patterns)]
				| _ => unreachable!("new PDU version"),
			}
		}

		#[allow(refining_impl_trait)]
		fn auth_events(&self) -> Box<dyn DoubleEndedIterator<Item = &Self::Id> + Send + '_> {
			match &self.rest {
				| Pdu::RoomV1Pdu(ev) => Box::new(ev.auth_events.iter().map(|(id, _)| id)),
				| Pdu::RoomV3Pdu(ev) => Box::new(ev.auth_events.iter()),
				#[allow(unreachable_patterns)]
				| _ => unreachable!("new PDU version"),
			}
		}

		fn redacts(&self) -> Option<&Self::Id> {
			match &self.rest {
				| Pdu::RoomV1Pdu(ev) => ev.redacts.as_ref(),
				| Pdu::RoomV3Pdu(ev) => ev.redacts.as_ref(),
				#[allow(unreachable_patterns)]
				| _ => unreachable!("new PDU version"),
			}
		}
	}

	#[derive(Clone, Debug, Deserialize, Serialize)]
	#[allow(clippy::exhaustive_structs)]
	pub(crate) struct PduEvent {
		pub(crate) event_id: OwnedEventId,
		#[serde(flatten)]
		pub(crate) rest: Pdu,
	}
}
