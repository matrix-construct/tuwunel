//! State-resolution load benchmarks reproducing the workload of Complement's
//! `TestMSC4297StateResolutionV2_1_*` suite: a deep `m.room.member` chain plus
//! a conflicting fork on a power-relevant state event, resolved under both v2.0
//! and the v2.1 algorithm (empty-start iterative auth + conflicted subgraph).
//!
//! The chain length drives the auth difference, the conflicted-subgraph walk,
//! and the per-event iterative auth checks, which is where the suite spends its
//! time. v2.1 is selected with the `hydra_backports` flag rather than a v12
//! room version, so both arms run over identical input.
#![cfg(test)]

use std::{
	collections::{BTreeSet, HashMap},
	iter::{from_fn, once},
};

use criterion::{
	BatchSize, BenchmarkGroup, BenchmarkId, Criterion, Throughput,
	async_executor::FuturesExecutor, criterion_group, criterion_main, measurement::Measurement,
};
use ruma::{
	OwnedEventId, RoomVersionId, UserId, events::TimelineEventType,
	room_version_rules::RoomVersionRules, user_id,
};
use serde_json::{
	Value, json,
	value::{RawValue as RawJsonValue, to_raw_value as to_raw_json_value},
};
use tuwunel_core::{
	matrix::{Event, PduEvent, event::TypeExt},
	smallstr::SmallString,
	smallvec::SmallVec,
	utils::stream::IterStream,
};
use tuwunel_service::rooms::state_res::{
	AuthSet, StateMap, resolve,
	test_utils::{alice, bob, charlie, event_id, to_pdu_event},
};

type Label = SmallString<[u8; 8]>;
type AuthRefs<'a> = SmallVec<[&'a str; 4]>;
type Events = HashMap<OwnedEventId, PduEvent>;
type Rows = HashMap<OwnedEventId, Vec<u8>>;
type States = Vec<StateMap<OwnedEventId>>;
type AuthSets = Vec<AuthSet<OwnedEventId>>;

const CHAIN_LENGTHS: [usize; 4] = [16, 64, 256, 1024];

criterion_group!(benches, problem_a, problem_b);

criterion_main!(benches);

fn problem_a<M: Measurement + 'static>(c: &mut Criterion<M>) {
	bench_scenario(c, "msc4297_problem_a", build_problem_a);
}

fn problem_b<M: Measurement + 'static>(c: &mut Criterion<M>) {
	bench_scenario(c, "msc4297_problem_b", build_problem_b);
}

fn bench_scenario<M: Measurement + 'static>(
	c: &mut Criterion<M>,
	name: &str,
	build: fn(usize) -> (Events, States),
) {
	let mut group = c.benchmark_group(name); // Criterion registration requires mutable access.
	let rules = RoomVersionId::V11
		.rules()
		.expect("room version v11 should be supported");

	for chain in CHAIN_LENGTHS {
		let (events, states) = build(chain);
		let auth_sets = auth_chain_sets(&events, &states);
		let rows: Rows = events
			.iter()
			.map(|(id, event)| {
				(id.clone(), serde_json::to_vec(event).expect("event should serialize"))
			})
			.collect();

		group.throughput(Throughput::Elements(
			u64::try_from(chain).expect("chain length fits u64"),
		));

		bench_versions(&mut group, chain, &rows, &states, &auth_sets, &rules);
	}

	group.finish();
}

fn bench_versions<M: Measurement + 'static>(
	group: &mut BenchmarkGroup<'_, M>,
	chain: usize,
	rows: &Rows,
	states: &[StateMap<OwnedEventId>],
	auth_sets: &[AuthSet<OwnedEventId>],
	rules: &RoomVersionRules,
) {
	for (label, hydra_backports) in [("v2.0", false), ("v2.1", true)] {
		group.bench_with_input(BenchmarkId::new(label, chain), &chain, |b, _| {
			b.to_async(FuturesExecutor).iter_batched(
				|| (states.to_vec(), auth_sets.to_vec()),
				|(states, auth_sets)| {
					run_resolve(rows, states, auth_sets, rules, hydra_backports)
				},
				BatchSize::SmallInput,
			);
		});
	}
}

async fn run_resolve(
	rows: &Rows,
	states: States,
	auth_sets: AuthSets,
	rules: &RoomVersionRules,
	hydra_backports: bool,
) {
	resolve(
		rules,
		states.into_iter().stream(),
		auth_sets.into_iter().stream(),
		rows,
		hydra_backports,
	)
	.await
	.expect("state resolution should succeed");
}

fn build_problem_a(chain: usize) -> (Events, States) {
	let refs: &[&str] = &[];
	let content = room_version();
	let event = to_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		content,
		refs,
		&[],
	);

	let event_0 = event;

	let event_1 = member("IMA", alice(), &["CREATE"], &["CREATE"]);
	let content = raw(json!({ "users": { alice(): 100, bob(): 50, charlie(): 50 } }));
	let event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		content,
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let event_2 = event;

	let event_3 = join_rule("IJR", "public", &["CREATE", "IMA", "IPOWER"], &["IPOWER"]);
	let event_4 = member("IMB", bob(), &["CREATE", "IJR", "IPOWER"], &["IJR"]);
	let event_5 = member("IMC", charlie(), &["CREATE", "IJR", "IPOWER"], &["IMB"]);
	let event_6 = join_rule("JRINV", "invite", &["CREATE", "IMA", "IPOWER"], &["IMC"]);

	let events = once(event_0)
		.chain(once(event_1))
		.chain(once(event_2))
		.chain(once(event_3))
		.chain(once(event_4))
		.chain(once(event_5))
		.chain(once(event_6))
		.map(|event| (event.event_id().to_owned(), event))
		.collect();

	let (events, tip) =
		append_member_chain(events, charlie(), "IMC", &["CREATE", "IPOWER", "JRINV"], chain);

	let current = state_map(&events, &["CREATE", "IMA", "IPOWER", "JRINV", "IMB", &tip]);
	let stale = state_map(&events, &["CREATE", "IMA", "IPOWER", "IJR", "IMB", "IMC"]);

	(events, vec![current, stale])
}

fn build_problem_b(chain: usize) -> (Events, States) {
	let refs: &[&str] = &[];
	let content = room_version();
	let event = to_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		content,
		refs,
		&[],
	);

	let event_0 = event;

	let event_1 = member("IMA", alice(), &["CREATE"], &["CREATE"]);
	let event =
		power_levels("PL1", json!({ "users": { alice(): 100 } }), &["CREATE", "IMA"], &["IMA"]);

	let event_2 = event;

	let event_3 = join_rule("IJR", "public", &["CREATE", "IMA", "PL1"], &["PL1"]);
	let event_4 = member("IMB", bob(), &["CREATE", "IJR", "PL1"], &["IJR"]);
	let event = power_levels(
		"PL2",
		json!({ "users": { alice(): 100, bob(): 50 } }),
		&["CREATE", "IMA", "PL1"],
		&["IMB"],
	);

	let event_5 = event;

	let event_6 = member("IMC", charlie(), &["CREATE", "IJR", "PL2"], &["PL2"]);
	let event = power_levels(
		"PL3",
		json!({ "users": { alice(): 100, bob(): 50, charlie(): 50 } }),
		&["CREATE", "IMA", "PL2"],
		&["IMC"],
	);

	let event_7 = event;

	let event_8 = member("IME", eve(), &["CREATE", "IJR", "PL3"], &["PL3"]);

	let events = once(event_0)
		.chain(once(event_1))
		.chain(once(event_2))
		.chain(once(event_3))
		.chain(once(event_4))
		.chain(once(event_5))
		.chain(once(event_6))
		.chain(once(event_7))
		.chain(once(event_8))
		.map(|event| (event.event_id().to_owned(), event))
		.collect();

	let (events, tip) =
		append_member_chain(events, eve(), "IME", &["CREATE", "PL3", "IJR"], chain);

	let current = state_map(&events, &["CREATE", "IMA", "PL3", "IJR", "IMB", "IMC", &tip]);
	let stale = state_map(&events, &["CREATE", "IMA", "PL1", "IJR", "IMB", "IMC", "IME"]);

	(events, vec![current, stale])
}

fn append_member_chain(
	events: Events,
	sender: &UserId,
	root: &str,
	base_auth: &[&str],
	chain: usize,
) -> (Events, Label) {
	(0..chain).fold((events, Label::from_str(root)), |(events, prev), i| {
		let id = Label::from_str(&format!("M{i}"));
		let auth: AuthRefs<'_> = base_auth
			.iter()
			.copied()
			.chain([prev.as_str()])
			.collect();

		let content = raw(json!({ "membership": "join", "displayname": format!("name {i}") }));
		let event = to_pdu_event(
			&id,
			sender,
			TimelineEventType::RoomMember,
			Some(sender.as_str()),
			content,
			&auth,
			&[prev.as_str()],
		);

		(insert_event(events, event), id)
	})
}

fn insert_event(mut events: Events, event: PduEvent) -> Events {
	events.insert(event.event_id().to_owned(), event);
	events
}

fn auth_chain_sets(events: &Events, states: &[StateMap<OwnedEventId>]) -> AuthSets {
	states
		.iter()
		.map(|state| auth_set(events, state))
		.collect()
}

fn auth_set(events: &Events, state: &StateMap<OwnedEventId>) -> AuthSet<OwnedEventId> {
	let stack: Vec<_> = state.values().cloned().collect();
	let mut traversal = (BTreeSet::new(), stack); // FnMut retains the traversal cursor between calls.

	from_fn(move || {
		loop {
			let id = traversal.1.pop()?;
			if !traversal.0.insert(id.clone()) {
				continue;
			}

			traversal.1.extend(
				events
					.get(&id)
					.into_iter()
					.flat_map(|event| event.auth_events().map(ToOwned::to_owned)),
			);

			return Some(id);
		}
	})
	.collect()
}

fn state_map(events: &Events, ids: &[&str]) -> StateMap<OwnedEventId> {
	ids.iter()
		.map(|id| {
			let event = events
				.get(&event_id(id))
				.expect("state event should exist");

			let key = event
				.event_type()
				.with_state_key(event.state_key().expect("state event"));

			(key, event.event_id().to_owned())
		})
		.collect()
}

fn member(id: &str, sender: &UserId, auth: &[&str], prev: &[&str]) -> PduEvent {
	let content = raw(json!({ "membership": "join" }));

	to_pdu_event(
		id,
		sender,
		TimelineEventType::RoomMember,
		Some(sender.as_str()),
		content,
		auth,
		prev,
	)
}

fn join_rule(id: &str, rule: &str, auth: &[&str], prev: &[&str]) -> PduEvent {
	let content = raw(json!({ "join_rule": rule }));

	to_pdu_event(id, alice(), TimelineEventType::RoomJoinRules, Some(""), content, auth, prev)
}

fn power_levels(id: &str, users: Value, auth: &[&str], prev: &[&str]) -> PduEvent {
	let content = raw(users);

	to_pdu_event(id, alice(), TimelineEventType::RoomPowerLevels, Some(""), content, auth, prev)
}

fn room_version() -> Box<RawJsonValue> { raw(json!({ "room_version": "11" })) }

#[expect(clippy::needless_pass_by_value)] // serialize wrapper; callers pass json! temporaries
fn raw(value: Value) -> Box<RawJsonValue> {
	to_raw_json_value(&value).expect("value should serialize")
}

fn eve() -> &'static UserId { user_id!("@eve:foo") }
