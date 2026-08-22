//! Soft-fail auth-boundary integration tests.
#![cfg(test)]

use std::{collections::HashMap, fs::read_to_string, path::Path};

use ruma::{EventId, OwnedEventId, RoomVersionId, events::StateEventType};
use serde_json::from_str as from_json_str;
use tuwunel_core::{
	Result, err,
	matrix::{Event, Pdu, StateKey},
};
use tuwunel_service::rooms::state_res::{AuthCheckOutcome, auth_check};

type Events = HashMap<OwnedEventId, Pdu>;
type State = HashMap<StateEventType, HashMap<String, OwnedEventId>>;

#[tokio::test]
async fn delayed_branch_valid_leave_fails_only_current_state_auth() {
	let events = load_fixture();
	let incoming = event(&events, "$ghost_delayed_leave");
	let positional = state([
		event(&events, "$create"),
		event(&events, "$alice_join"),
		event(&events, "$power"),
		event(&events, "$join_rules"),
		event(&events, "$ghost_join"),
	]);

	let current = state([
		event(&events, "$create"),
		event(&events, "$alice_join"),
		event(&events, "$power"),
		event(&events, "$join_rules"),
		event(&events, "$ghost_current_leave"),
	]);

	let rules = RoomVersionId::V10
		.rules()
		.expect("room version should be supported");

	let fetch_event = async |event_id: OwnedEventId| get_event(&events, &event_id);
	let positional_fetch =
		async |event_type, state_key| get_state(&events, &positional, &event_type, &state_key);

	let current_fetch =
		async |event_type, state_key| get_state(&events, &current, &event_type, &state_key);

	let ghost_join = event(&events, "$ghost_join").event_id();
	assert!(
		incoming
			.prev_events()
			.any(|event_id| event_id == ghost_join)
	);
	assert!(
		event(&events, "$ghost_current_leave")
			.prev_events()
			.any(|event_id| event_id == ghost_join),
	);

	let positional = auth_check(&rules, incoming, &fetch_event, &positional_fetch)
		.await
		.expect("delayed leave should pass against its positional branch state");
	assert!(matches!(positional, AuthCheckOutcome::Allow));

	let current = auth_check(&rules, incoming, &fetch_event, &current_fetch)
		.await
		.expect("current-state denial should remain evaluable");
	assert!(matches!(current, AuthCheckOutcome::Deny(_)));
}

fn load_fixture() -> Events {
	let path = Path::new("tests/state_res/fixtures/soft-fail-split.json");
	let json = read_to_string(path).expect("soft-fail fixture should be readable");

	from_json_str::<Vec<Pdu>>(&json)
		.expect("soft-fail fixture should contain valid PDUs")
		.into_iter()
		.map(|event| (event.event_id().to_owned(), event))
		.collect()
}

fn event<'a>(events: &'a Events, event_id: &str) -> &'a Pdu {
	let event_id = <&EventId>::try_from(event_id).expect("fixture event ID should be valid");

	events
		.get(event_id)
		.expect("fixture event should be present")
}

#[allow(single_use_lifetimes, clippy::allow_attributes)]
fn state<'a>(events: impl IntoIterator<Item = &'a Pdu>) -> State {
	events
		.into_iter()
		.fold(State::new(), |mut state, event| {
			let event_type = StateEventType::from(event.event_type().to_string());
			let state_key = event
				.state_key()
				.expect("fixture state event should have a state key")
				.to_owned();

			state
				.entry(event_type)
				.or_default()
				.insert(state_key, event.event_id().to_owned());

			state
		})
}

fn get_event(events: &Events, event_id: &EventId) -> Result<Pdu> {
	events
		.get(event_id)
		.cloned()
		.ok_or_else(|| err!(Request(NotFound("fixture event not found"))))
}

fn get_state(
	events: &Events,
	state: &State,
	event_type: &StateEventType,
	state_key: &StateKey,
) -> Result<Pdu> {
	let event_id = state
		.get(event_type)
		.and_then(|events| events.get(state_key.as_str()))
		.ok_or_else(|| err!(Request(NotFound("fixture state event not found"))))?;

	get_event(events, event_id)
}
