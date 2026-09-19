#![cfg(test)]

use std::collections::HashMap;

use criterion::{Criterion, async_executor::FuturesExecutor, criterion_group, criterion_main};
use maplit::hashmap;
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedEventId, RoomVersionId, events::TimelineEventType, int, uint,
};
use serde_json::{json, value::to_raw_value as to_raw_json_value};
use tuwunel_core::{
	matrix::{Event, PduEvent, event::TypeExt},
	utils::stream::IterStream,
};
use tuwunel_service::rooms::state_res::{
	AuthSet, StateMap, resolve,
	test_utils::{
		INITIAL_EVENTS, TestStore, alice, bob, ella, event_id, member_content_ban,
		member_content_join, room_id, to_pdu_event,
	},
	topological_sort,
};

type Rows = HashMap<OwnedEventId, Vec<u8>>;

criterion_group!(
	benches,
	lexico_topo_sort,
	resolution_shallow_auth_chain,
	resolve_deeper_event_set
);

criterion_main!(benches);

#[expect(
	clippy::iter_on_single_items,
	clippy::iter_on_empty_collections
)]
fn lexico_topo_sort(c: &mut Criterion) {
	c.bench_function("lexico_topo_sort", |c| {
		let graph = hashmap! {
			event_id("l") => [event_id("o")].into_iter().collect(),
			event_id("m") => [event_id("n"), event_id("o")].into_iter().collect(),
			event_id("n") => [event_id("o")].into_iter().collect(),
			event_id("o") => [].into_iter().collect(), // "o" has zero outgoing edges but 4 incoming edges
			event_id("p") => [event_id("o")].into_iter().collect(),
		};

		c.to_async(FuturesExecutor).iter(async || {
			_ = topological_sort(graph.clone(), &async |_id| {
				Ok((int!(0).into(), MilliSecondsSinceUnixEpoch(uint!(0))))
			})
			.await;
		});
	});
}

fn resolution_shallow_auth_chain(c: &mut Criterion) {
	c.bench_function("resolution_shallow_auth_chain", |c| {
		let mut store = TestStore(maplit::hashmap! {});

		// build up the DAG
		let (state_at_bob, state_at_charlie, _) = store.set_up();

		let rules = RoomVersionId::V6.rules().unwrap();
		let rows = rows(&store.0);
		let state_sets = [state_at_bob, state_at_charlie];
		let auth_chains = auth_chains(&store, &state_sets);

		let func = async || {
			if let Err(e) = resolve(
				&rules,
				state_sets.clone().into_iter().stream(),
				auth_chains.clone().into_iter().stream(),
				&rows,
				false,
			)
			.await
			{
				panic!("{e}")
			}
		};

		c.to_async(FuturesExecutor).iter(async || {
			func().await;
		});
	});
}

fn rows(events: &HashMap<OwnedEventId, PduEvent>) -> Rows {
	events
		.iter()
		.map(|(id, event)| {
			let row = serde_json::to_vec(event).expect("fixture event should serialize");

			(id.clone(), row)
		})
		.collect()
}

fn auth_chains(
	store: &TestStore,
	state_sets: &[StateMap<OwnedEventId>],
) -> Vec<AuthSet<OwnedEventId>> {
	state_sets
		.iter()
		.map(|map| {
			store
				.auth_event_ids(room_id(), map.values().cloned().collect())
				.unwrap()
		})
		.collect()
}

fn resolve_deeper_event_set(c: &mut Criterion) {
	c.bench_function("resolver_deeper_event_set", |c| {
		let mut inner = INITIAL_EVENTS();
		let ban = BAN_STATE_SET();

		inner.extend(ban);
		let store = TestStore(inner.clone());

		let state_set_a = [
			&inner[&event_id("CREATE")],
			&inner[&event_id("IJR")],
			&inner[&event_id("IMA")],
			&inner[&event_id("IMB")],
			&inner[&event_id("IMC")],
			&inner[&event_id("MB")],
			&inner[&event_id("PA")],
		]
		.iter()
		.map(|ev| {
			(
				ev.event_type()
					.with_state_key(ev.state_key().unwrap()),
				ev.event_id().to_owned(),
			)
		})
		.collect::<StateMap<_>>();

		let state_set_b = [
			&inner[&event_id("CREATE")],
			&inner[&event_id("IJR")],
			&inner[&event_id("IMA")],
			&inner[&event_id("IMB")],
			&inner[&event_id("IMC")],
			&inner[&event_id("IME")],
			&inner[&event_id("PA")],
		]
		.iter()
		.map(|ev| {
			(
				ev.event_type()
					.with_state_key(ev.state_key().unwrap()),
				ev.event_id().to_owned(),
			)
		})
		.collect::<StateMap<_>>();

		let rules = RoomVersionId::V6.rules().unwrap();
		let state_sets = [state_set_a, state_set_b];
		let auth_chains = auth_chains(&store, &state_sets);

		let rows = rows(&inner);

		let func = async || {
			if let Err(e) = resolve(
				&rules,
				state_sets.clone().into_iter().stream(),
				auth_chains.clone().into_iter().stream(),
				&rows,
				false,
			)
			.await
			{
				panic!("{e}")
			}
		};

		c.to_async(FuturesExecutor).iter(async || {
			func().await;
		});
	});
}

// all graphs start with these input events
#[expect(non_snake_case)]
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
	.map(|ev| (ev.event_id().to_owned(), ev))
	.collect()
}
