//! State resolution integration tests.
#![cfg(test)]

use std::{
	cmp::Ordering,
	collections::{BTreeSet, HashMap},
	error::Error,
	fs,
	path::Path,
};

use ruma::{
	OwnedEventId, RoomVersionId,
	events::{StateEventType, TimelineEventType},
	room_version_rules::{AuthorizationRules, RoomVersionRules, StateResolutionV2Rules},
};
use serde::{Deserialize, Serialize};
use serde_json::{
	Error as JsonError, Value as JsonValue, from_str as from_json_str,
	to_string_pretty as to_json_string_pretty, to_value as to_json_value,
};
use similar::{Algorithm, udiff::unified_diff};
use tracing_subscriber::EnvFilter;
use tuwunel_core::{
	Result, err,
	matrix::{Event, Pdu, StateKey},
	utils::stream::IterStream,
};
use tuwunel_service::rooms::state_res::{AuthSet, StateMap, resolve};

/// Create a new snapshot test.
///
/// # Arguments
///
/// * The test function's name.
/// * A list of JSON files relative to `tests/state_res/fixtures` to load PDUs
///   to resolve from.
macro_rules! snapshot_test {
    ($name:ident, $paths:expr $(,)?) => {
        #[tokio::test]
        async fn $name() {
            let crate::resolve::Snapshots {
                resolved_state,
            } = crate::resolve::test_resolve(&$paths).await;

            insta::with_settings!({
                description => "Resolved state",
                omit_expression => true,
                snapshot_suffix => "resolved_state",
            }, {
                insta::assert_json_snapshot!(&resolved_state);
            });
        }
    };
}

/// Create a new snapshot test, attempting to resolve multiple contrived states.
///
/// # Arguments
///
/// * The test function's name.
/// * A list of JSON files relative to `tests/state_res/fixtures` to load PDUs
///   to resolve from.
/// * A list of JSON files relative to `tests/state_res/fixtures` to load event
///   IDs forming contrived states to resolve.
macro_rules! snapshot_test_contrived_states {
    ($name:ident, $pdus_path:expr, $state_set_paths:expr $(,)?) => {
        #[tokio::test]
        async fn $name() {
            let crate::resolve::Snapshots {
                resolved_state,
            } = crate::resolve::test_contrived_states(&$pdus_path, &$state_set_paths).await;

            insta::with_settings!({
                description => "Resolved state",
                omit_expression => true,
                snapshot_suffix => "resolved_state",
            }, {
                insta::assert_json_snapshot!(&resolved_state);
            });
        }
    };
}

// This module must be defined lexically after the `snapshot_test` macro.
mod snapshot_tests;

/// Extract `.content.room_version` from a PDU.
#[derive(Deserialize)]
struct ExtractRoomVersion {
	room_version: RoomVersionId,
}

/// Type describing a resolved state event.
#[derive(Serialize)]
struct ResolvedStateEvent {
	kind: StateEventType,
	state_key: StateKey,
	event_id: OwnedEventId,

	// Ignored in `PartialEq` and `Ord` because we don't want to consider it while sorting.
	content: JsonValue,
}

impl PartialEq for ResolvedStateEvent {
	fn eq(&self, other: &Self) -> bool {
		self.kind == other.kind
			&& self.state_key == other.state_key
			&& self.event_id == other.event_id
	}
}

impl Eq for ResolvedStateEvent {}

impl Ord for ResolvedStateEvent {
	fn cmp(&self, other: &Self) -> Ordering {
		Ordering::Equal
			.then(self.kind.cmp(&other.kind))
			.then(self.state_key.cmp(&other.state_key))
			.then(self.event_id.cmp(&other.event_id))
	}
}

impl PartialOrd for ResolvedStateEvent {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

/// Information to be captured in snapshot assertions
struct Snapshots {
	/// The resolved state of the room.
	resolved_state: BTreeSet<ResolvedStateEvent>,
}

fn snapshot_test_prelude(
	paths: &[&str],
) -> (Vec<Vec<Pdu>>, RoomVersionRules, AuthorizationRules, StateResolutionV2Rules) {
	// Run `cargo test -- --show-output` to view traces, set `RUST_LOG` to control
	// filtering.
	let subscriber = tracing_subscriber::fmt()
		.with_env_filter(EnvFilter::from_default_env())
		.with_test_writer()
		.with_span_events(tracing_subscriber::fmt::format::FmtSpan::ACTIVE)
		.finish();

	tracing::subscriber::set_global_default(subscriber).ok();

	let fixtures_path = Path::new("tests/state_res/fixtures");

	let pdu_batches = paths
		.iter()
		.map(|x| {
			from_json_str(
				&fs::read_to_string(fixtures_path.join(x))
					.expect("should be able to read JSON file of PDUs"),
			)
			.expect("should be able to deserialize JSON file of PDUs")
		})
		.collect::<Vec<Vec<Pdu>>>();

	let room_version_id = {
		let first_pdu = pdu_batches
			.first()
			.expect("there should be at least one file of PDUs")
			.first()
			.expect("there should be at least one PDU in the first file");

		assert_eq!(
			first_pdu.kind,
			TimelineEventType::RoomCreate,
			"the first PDU in the first file should be an m.room.create event",
		);

		from_json_str::<ExtractRoomVersion>(first_pdu.content.get())
			.expect("the m.room.create PDU's content should be valid")
			.room_version
	};
	let rules = room_version_id
		.rules()
		.expect("room version should be supported");
	let auth_rules = rules.clone().authorization;
	let state_res_rules = rules
		.state_res
		.v2_rules()
		.copied()
		.expect("resolve only supports state resolution version 2");

	(pdu_batches, rules, auth_rules, state_res_rules)
}

/// Reshape the data a bit to make the diff and snapshots easier to compare.
fn reshape(
	pdus_by_id: &HashMap<OwnedEventId, Pdu>,
	x: StateMap<OwnedEventId>,
) -> Result<BTreeSet<ResolvedStateEvent>, JsonError> {
	x.into_iter()
		.map(|((kind, state_key), event_id)| {
			Ok(ResolvedStateEvent {
				kind,
				state_key,
				content: to_json_value(pdus_by_id[&event_id].content())?,
				event_id,
			})
		})
		.collect()
}

/// Test a list of JSON files containing a list of PDUs and return the results.
///
/// State resolution is run both atomically for all PDUs and in batches of PDUs
/// by file.
async fn test_resolve(paths: &[&str]) -> Snapshots {
	let (pdu_batches, rules, auth_rules, state_res_rules) = snapshot_test_prelude(paths);

	// Resolve PDUs iteratively, using the ordering of `prev_events`.
	let iteratively_resolved_state = resolve_iteratively(
		&rules,
		&auth_rules,
		&state_res_rules,
		pdu_batches.iter().flat_map(|x| x.iter()),
	)
	.await
	.expect("iterative state resolution should succeed");

	// Resolve PDUs in batches by file
	let mut pdus_by_id = HashMap::new();
	let mut batched_resolved_state = None;
	for pdus in &pdu_batches {
		batched_resolved_state = Some(
			resolve_batch(
				&rules,
				&auth_rules,
				&state_res_rules,
				pdus,
				&mut pdus_by_id,
				&mut batched_resolved_state,
			)
			.await
			.expect("batched state resolution step should succeed"),
		);
	}
	let batched_resolved_state =
		batched_resolved_state.expect("batched state resolution should have run at least once");

	// Resolve all PDUs in a single step
	let atomic_resolved_state = resolve_batch(
		&rules,
		&auth_rules,
		&state_res_rules,
		pdu_batches.iter().flat_map(|x| x.iter()),
		&mut HashMap::new(),
		&mut None,
	)
	.await
	.expect("atomic state resolution should succeed");

	let iteratively_resolved_state = reshape(&pdus_by_id, iteratively_resolved_state)
		.expect("should be able to reshape iteratively resolved state");
	let batched_resolved_state = reshape(&pdus_by_id, batched_resolved_state)
		.expect("should be able to reshape batched resolved state");
	let atomic_resolved_state = reshape(&pdus_by_id, atomic_resolved_state)
		.expect("should be able to reshape atomic resolved state");

	let assert_states_match = |first_resolved_state: &BTreeSet<ResolvedStateEvent>,
	                           second_resolved_state: &BTreeSet<ResolvedStateEvent>,
	                           first_name: &str,
	                           second_name: &str| {
		if first_resolved_state != second_resolved_state {
			let diff = unified_diff(
				Algorithm::default(),
				&to_json_string_pretty(first_resolved_state)
					.expect("should be able to serialize first resolved state"),
				&to_json_string_pretty(second_resolved_state)
					.expect("should be able to serialize second resolved state"),
				3,
				Some((first_name, second_name)),
			);

			panic!(
				"{first_name} and {second_name} results should match; but they differ:\n{diff}"
			);
		}
	};

	assert_states_match(
		&iteratively_resolved_state,
		&batched_resolved_state,
		"iterative",
		"batched",
	);
	assert_states_match(&batched_resolved_state, &atomic_resolved_state, "batched", "atomic");

	Snapshots {
		resolved_state: iteratively_resolved_state,
	}
}

/// Test a list of JSON files containing a list of PDUs and a list of JSON files
/// containing the event IDs that form a contrived state and return the results.
#[tracing::instrument(parent = None, name = "test", skip_all)]
async fn test_contrived_states(pdus_paths: &[&str], state_sets_paths: &[&str]) -> Snapshots {
	let (pdu_batches, rules, _auth_rules, _state_res_rules) = snapshot_test_prelude(pdus_paths);

	let pdus = pdu_batches
		.into_iter()
		.flat_map(IntoIterator::into_iter)
		.collect::<Vec<_>>();

	let pdus_by_id: HashMap<OwnedEventId, Pdu> = pdus
		.clone()
		.into_iter()
		.map(|pdu| (pdu.event_id().to_owned(), pdu.clone()))
		.collect();

	let fixtures_path = Path::new("tests/state_res/fixtures");

	let state_sets = state_sets_paths
		.iter()
		.map(|x| {
			from_json_str::<Vec<OwnedEventId>>(
				&fs::read_to_string(fixtures_path.join(x))
					.expect("should be able to read JSON file of PDUs"),
			)
			.expect("should be able to deserialize JSON file of PDUs")
			.into_iter()
			.map(|event_id| {
				pdus_by_id
					.get(&event_id)
					.map(|pdu| {
						(
							(
								pdu.event_type().to_string().into(),
								pdu.state_key
									.clone()
									.expect("All PDUs must be state events"),
							),
							event_id,
						)
					})
					.expect("Event IDs in JSON file must be in PDUs JSON")
			})
			.collect()
		})
		.collect::<Vec<StateMap<OwnedEventId>>>();

	let mut auth_chain_sets = Vec::new();
	for state_map in &state_sets {
		let mut auth_chain = AuthSet::new();

		for event_id in state_map.values() {
			let pdu = pdus_by_id
				.get(event_id)
				.expect("We already confirmed all state set event ids have pdus");

			auth_chain.extend(
				auth_events_dfs(&pdus_by_id, pdu).expect("Auth events DFS should not fail"),
			);
		}

		auth_chain_sets.push(auth_chain);
	}

	let exists = async |x| pdus_by_id.contains_key(&x);
	let fetch = async |x| {
		pdus_by_id
			.get(&x)
			.cloned()
			.ok_or_else(|| err!(Request(NotFound("event not found"))))
	};

	let resolved_state = resolve(
		&rules,
		state_sets.into_iter().stream(),
		auth_chain_sets.into_iter().stream(),
		&fetch,
		&exists,
		false,
	)
	.await
	.expect("atomic state resolution should succeed");

	Snapshots {
		resolved_state: reshape(&pdus_by_id, resolved_state)
			.expect("should be able to reshape atomic resolved state"),
	}
}

/// Perform state resolution on a batch of PDUs.
///
/// This function can be used to resolve the state of a room in a single call if
/// all PDUs are provided at once, or across multiple calls if given PDUs in
/// batches in a loop. The latter form simulates the case commonly experienced
/// by homeservers during normal operation.
///
/// # Arguments
///
/// * `rules`: The rules of the room version.
/// * `pdus`: An iterator of [`Pdu`]s to resolve, either alone or against the
///   `prev_state`.
/// * `pdus_by_id`: A map of [`OwnedEventId`]s to the [`Pdu`] with that ID.
///   * Should be empty for the first call.
///   * Should not be mutated outside of this function.
/// * `prev_state`: The state returned by a previous call to this function, if
///   any.
///   * Should be `None` for the first call.
///   * Should not be mutated outside of this function.
async fn resolve_batch<'a, I, II>(
	rules: &'a RoomVersionRules,
	_auth_rules: &'a AuthorizationRules,
	_state_res_rules: &'a StateResolutionV2Rules,
	pdus: II,
	pdus_by_id: &'a mut HashMap<OwnedEventId, Pdu>,
	prev_state: &'a mut Option<StateMap<OwnedEventId>>,
) -> Result<StateMap<OwnedEventId>, Box<dyn Error>>
where
	I: Iterator<Item = &'a Pdu> + Send + 'a,
	II: IntoIterator<IntoIter = I> + Clone + Send + 'a,
	Pdu: Send + Sync + 'a,
	&'a Pdu: Send + 'a,
{
	let mut state_sets = prev_state
		.take()
		.map(|x| vec![x])
		.unwrap_or_default();

	for pdu in pdus.clone() {
		// Insert each state event into its own StateMap because we don't know any valid
		// groupings.
		let mut state_map = StateMap::new();

		state_map.insert(
			(
				pdu.event_type().to_string().into(),
				pdu.state_key()
					.ok_or("all PDUs should be state events")?
					.into(),
			),
			pdu.event_id().to_owned(),
		);

		state_sets.push(state_map);
	}

	pdus_by_id.extend(
		pdus.clone()
			.into_iter()
			.map(|pdu| (pdu.event_id().to_owned(), pdu.to_owned())),
	);

	let mut auth_chain_sets = Vec::new();
	for pdu in pdus {
		auth_chain_sets.push(auth_events_dfs(&*pdus_by_id, pdu)?);
	}

	let fetch = async |x| {
		pdus_by_id
			.get(&x)
			.cloned()
			.ok_or_else(|| err!(Request(NotFound("event not found"))))
	};

	let exists = async |x| pdus_by_id.contains_key(&x);

	resolve(
		rules,
		state_sets.into_iter().stream(),
		auth_chain_sets.into_iter().stream(),
		&fetch,
		&exists,
		false,
	)
	.await
	.map_err(Into::into)
}

/// Perform state resolution on a batch of PDUs iteratively, one-by-one.
///
/// This function walks the `prev_events` of each PDU forward, resolving each
/// pdu against the state(s) of it's `prev_events`, to emulate what would happen
/// in a regular room a server is participating in.
///
/// # Arguments
///
/// * `auth_rules`: The authorization rules of the room version.
/// * `state_res_rules`: The state resolution rules of the room version.
/// * `pdus`: An iterator of [`Pdu`]s to resolve, with the following
///   assumptions:
///   * `prev_events` of each PDU points to another provided state event.
///
/// # Returns
///
/// The state resolved by resolving all the leaves (PDUs which don't have any
/// other PDUs pointing to it via `prev_events`).
async fn resolve_iteratively<'a, I, II>(
	rules: &'a RoomVersionRules,
	_auth_rules: &'a AuthorizationRules,
	_state_res_rules: &'a StateResolutionV2Rules,
	pdus: II,
) -> Result<StateMap<OwnedEventId>, Box<dyn Error>>
where
	I: Iterator<Item = &'a Pdu>,
	II: IntoIterator<IntoIter = I> + Clone,
{
	let mut forward_prev_events_graph: HashMap<OwnedEventId, Vec<_>> = HashMap::new();
	let mut stack = Vec::new();

	for pdu in pdus.clone() {
		let mut has_prev_events = false;
		for prev_event in pdu.prev_events() {
			forward_prev_events_graph
				.entry(prev_event.into())
				.or_default()
				.push(pdu.event_id().into());

			has_prev_events = true;
		}

		if pdu.event_type() == &TimelineEventType::RoomCreate && !has_prev_events {
			stack.push(pdu.event_id().to_owned());
		}
	}

	let pdus_by_id: HashMap<OwnedEventId, Pdu> = pdus
		.clone()
		.into_iter()
		.map(|pdu| (pdu.event_id().to_owned(), pdu.to_owned()))
		.collect();

	let exists = async |x| pdus_by_id.contains_key(&x);
	let fetch = async |x| {
		pdus_by_id
			.get(&x)
			.cloned()
			.ok_or_else(|| err!(Request(NotFound("event not found"))))
	};

	let mut state_at_events: HashMap<OwnedEventId, StateMap<OwnedEventId>> = HashMap::new();
	let mut leaves = Vec::new();

	'outer: while let Some(event_id) = stack.pop() {
		let mut states_before_event = Vec::new();
		let mut auth_chain_sets = Vec::new();

		let current_pdu = pdus_by_id
			.get(&event_id)
			.expect("every pdu should be available");

		for prev_event in current_pdu.prev_events() {
			let Some(state_at_event) = state_at_events.get(prev_event) else {
				// State for a prev event is not known, we will come back to this event on a
				// later iteration.
				continue 'outer;
			};

			for pdu in state_at_event.values().map(|event_id| {
				pdus_by_id
					.get(event_id)
					.expect("every pdu should be available")
			}) {
				auth_chain_sets.push(auth_events_dfs(&pdus_by_id, pdu)?);
			}

			states_before_event.push(state_at_event.clone());
		}

		if states_before_event.is_empty() {
			// initial event, nothing to resolve
			state_at_events.insert(
				event_id.clone(),
				StateMap::from_iter([(
					(
						current_pdu.event_type().to_string().into(),
						current_pdu
							.state_key()
							.expect("all pdus are state events")
							.into(),
					),
					event_id.clone(),
				)]),
			);
		} else {
			let state_before_event = resolve(
				rules,
				states_before_event.clone().into_iter().stream(),
				auth_chain_sets.clone().into_iter().stream(),
				&fetch,
				&exists,
				false,
			)
			.await?;

			let mut proposed_state_at_event = state_before_event.clone();
			proposed_state_at_event.insert(
				(
					current_pdu.event_type().to_string().into(),
					current_pdu
						.state_key()
						.expect("all pdus are state events")
						.into(),
				),
				event_id.clone(),
			);

			auth_chain_sets.push(auth_events_dfs(&pdus_by_id, current_pdu)?);

			let state_at_event = resolve(
				rules,
				[state_before_event, proposed_state_at_event]
					.into_iter()
					.stream(),
				auth_chain_sets.into_iter().stream(),
				&fetch,
				&exists,
				false,
			)
			.await?;

			state_at_events.insert(event_id.clone(), state_at_event);
		}

		if let Some(prev_events) = forward_prev_events_graph.get(&event_id) {
			stack.extend(prev_events.iter().cloned());
		} else {
			// pdu is a leaf: no `prev_events` point to it.
			leaves.push(event_id);
		}
	}

	assert!(
		state_at_events.len() == pdus_by_id.len(),
		"Not all events have a state calculated! This is likely due to an event having a \
		 `prev_events` which points to a non-existent PDU."
	);

	let mut leaf_states = Vec::new();
	let mut auth_chain_sets = Vec::new();

	for leaf in leaves {
		let state_at_event = state_at_events
			.get(&leaf)
			.expect("states at all events are known");

		for pdu in state_at_event.values().map(|event_id| {
			pdus_by_id
				.get(event_id)
				.expect("every pdu should be available")
		}) {
			auth_chain_sets.push(auth_events_dfs(&pdus_by_id, pdu)?);
		}

		leaf_states.push(state_at_event.clone());
	}

	resolve(
		rules,
		leaf_states.into_iter().stream(),
		auth_chain_sets.into_iter().stream(),
		&fetch,
		&exists,
		false,
	)
	.await
	.map_err(Into::into)
}

/// Depth-first search for the `auth_events` of the given PDU.
///
/// # Errors
///
/// Fails if `pdus` does not contain a PDU that appears in the recursive
/// `auth_events` of `pdu`.
fn auth_events_dfs(
	pdus_by_id: &HashMap<OwnedEventId, Pdu>,
	pdu: &Pdu,
) -> Result<AuthSet<OwnedEventId>, Box<dyn Error>> {
	let mut out = AuthSet::new();
	let mut stack = pdu
		.auth_events()
		.map(ToOwned::to_owned)
		.collect::<Vec<_>>();

	while let Some(event_id) = stack.pop() {
		if out.contains(&event_id) {
			continue;
		}

		out.insert(event_id.clone());

		stack.extend(
			pdus_by_id
				.get(&event_id)
				.ok_or_else(|| format!("missing required PDU: {event_id}"))?
				.auth_events()
				.map(ToOwned::to_owned),
		);
	}

	Ok(out)
}
