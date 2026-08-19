use std::io::Error as IoError;

use ruma::{
	api::error::ErrorKind::InvalidParam,
	events::{
		StateEventType, TimelineEventType,
		room::{
			join_rules::{JoinRule, Restricted, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
			message::RoomMessageEventContent,
			redaction::RoomRedactionEventContent,
		},
	},
	int, owned_event_id, owned_room_id,
	room_version_rules::{AuthorizationRules, RoomVersionRules},
	uint, user_id,
};
use serde_json::{json, value::to_raw_value as to_raw_json_value};

mod room_power_levels;

use tuwunel_core::{
	Error, err,
	matrix::{Event, EventHash, PduEvent, StateKey},
};

use self::room_power_levels::default_room_power_levels;
use super::{
	AuthCheckOutcome, auth_check_outcome, check_room_create, check_room_redaction,
	check_state_dependent_auth_rules, check_state_independent_auth_rules, classify_auth_error,
	events::{RoomCreateEvent, RoomPowerLevelsEvent},
	test_utils::{
		INITIAL_EVENTS, INITIAL_HYDRA_EVENTS, TestStateMap, alice, charlie, ella, event_id,
		init_subscriber, member_content_join, not_found, room_create_hydra_pdu_event, room_id,
		room_redaction_pdu_event, room_third_party_invite, to_hydra_pdu_event, to_init_pdu_event,
		to_pdu_event, zara,
	},
};

#[tokio::test]
async fn auth_outcome_classifies_denials() {
	let _guard = init_subscriber();

	assert!(matches!(
		classify_auth_error(err!("clean denial")),
		Ok(AuthCheckOutcome::Deny(Error::Err(..)))
	));

	let init_events = INITIAL_EVENTS();
	let fetch_event = async |event_id| {
		init_events
			.get(&event_id)
			.cloned()
			.ok_or_else(not_found)
	};

	let state = TestStateMap::new(&init_events);
	let fetch_state = state.fetch_state_fn();

	let invalid = to_pdu_event(
		"INVALID",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&json!({})).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let outcome =
		auth_check_outcome(&RoomVersionRules::V6, &invalid, &fetch_event, &fetch_state).await;

	assert!(matches!(outcome, Ok(AuthCheckOutcome::Deny(Error::Request(InvalidParam, ..)))));

	let absent_member = to_pdu_event(
		"ABSENT_MEMBER",
		ella(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["IPOWER", "CREATE"],
		&["IPOWER"],
	);

	let outcome =
		auth_check_outcome(&RoomVersionRules::V6, &absent_member, &fetch_event, &fetch_state)
			.await;

	assert!(
		matches!(
			outcome,
			Ok(AuthCheckOutcome::Deny(Error::Err(reason)))
				if reason == "sender's membership `leave` is not `join`"
		),
		"absent membership produced the wrong denial"
	);

	let mut state_events = init_events.clone();

	state_events.remove(&event_id("IPOWER")).unwrap();

	let state = TestStateMap::new(&state_events);
	let fetch_state = state.fetch_state_fn();
	let absent_power_levels = to_pdu_event(
		"ABSENT_POWER",
		charlie(),
		TimelineEventType::RoomMessage,
		Some(""),
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["CREATE", "IMC", "IPOWER"],
		&["IPOWER"],
	);

	let outcome = auth_check_outcome(
		&RoomVersionRules::V6,
		&absent_power_levels,
		&fetch_event,
		&fetch_state,
	)
	.await;

	assert!(
		matches!(
			outcome,
			Ok(AuthCheckOutcome::Deny(Error::Err(reason)))
				if reason.starts_with("sender does not have enough power (")
					&& reason.ends_with(") for `m.room.message` event type (50)")
		),
		"absent power levels did not use the default power rules"
	);
}

#[tokio::test]
async fn auth_outcome_propagates_fetch_failures() {
	let _guard = init_subscriber();

	let incoming_event = allowed_message();
	let init_events = INITIAL_EVENTS();
	let fetch_event = async |event_id| {
		init_events
			.get(&event_id)
			.cloned()
			.ok_or_else(not_found)
	};

	let database_failure = async |_event_type: StateEventType, _state_key: StateKey| {
		Err(err!(Database("injected state failure")))
	};

	let outcome = auth_check_outcome(
		&RoomVersionRules::V6,
		&incoming_event,
		&fetch_event,
		&database_failure,
	)
	.await;

	assert!(matches!(outcome, Err(Error::Database(..))));

	let io_failure = async |_event_type: StateEventType, _state_key: StateKey| {
		Err(IoError::other("injected state failure").into())
	};

	let outcome =
		auth_check_outcome(&RoomVersionRules::V6, &incoming_event, &fetch_event, &io_failure)
			.await;

	assert!(matches!(outcome, Err(Error::Io(..))));

	let state = TestStateMap::new(&init_events);
	let fetch_state = state.fetch_state_fn();
	let missing_event = async |_event_id| Err(not_found());
	let outcome =
		auth_check_outcome(&RoomVersionRules::V6, &incoming_event, &missing_event, &fetch_state)
			.await;

	assert!(matches!(outcome, Err(error) if error.is_not_found()));
}

fn allowed_message() -> PduEvent {
	to_pdu_event(
		"ALLOWED",
		alice(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	)
}

#[test]
fn valid_room_create() {
	// Minimal fields valid for room v1.
	let content = json!({
		"creator": alice(),
	});
	let event = to_init_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&content).unwrap(),
	);
	check_room_create(&RoomCreateEvent::new(event), &AuthorizationRules::V1).unwrap();

	// Same, with room version.
	let content = json!({
		"creator": alice(),
		"room_version": "2",
	});
	let event = to_init_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&content).unwrap(),
	);
	check_room_create(&RoomCreateEvent::new(event), &AuthorizationRules::V1).unwrap();

	// With a room version that does not need the creator.
	let content = json!({
		"room_version": "11",
	});
	let event = to_init_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&content).unwrap(),
	);
	check_room_create(&RoomCreateEvent::new(event), &AuthorizationRules::V11).unwrap();

	// Check various contents that might not match the definition of `m.room.create`
	// in the spec, to ensure that we only care about a few fields.
	let contents_to_check = vec![
		// With an invalid predecessor, but we don't care about it. Inspired by a real-life
		// example.
		json!({
			"room_version": "11",
			"predecessor": "!XPoLiaavxVgyMSiRwK:localhost",
		}),
		// With an invalid type, but we don't care about it.
		json!({
			"room_version": "11",
			"type": true,
		}),
	];

	for content in contents_to_check {
		let event = to_init_pdu_event(
			"CREATE",
			alice(),
			TimelineEventType::RoomCreate,
			Some(""),
			to_raw_json_value(&content).unwrap(),
		);
		check_room_create(&RoomCreateEvent::new(event), &AuthorizationRules::V11).unwrap();
	}

	// Check `additional_creators` is allowed to contain invalid user IDs if the
	// room version doesn't acknowledge them.
	let content = json!({
		"room_version": "11",
		"additional_creators": ["@::example.org"]
	});
	let event = to_init_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&content).unwrap(),
	);
	check_room_create(&RoomCreateEvent::new(event), &AuthorizationRules::V11).unwrap();

	// Check `additional_creators` only contains valid user IDs.
	let content = json!({
		"room_version": "12",
		"additional_creators": ["@alice:example.org"]
	});
	let event =
		room_create_hydra_pdu_event("CREATE", alice(), to_raw_json_value(&content).unwrap());
	check_room_create(&RoomCreateEvent::new(event), &AuthorizationRules::V12).unwrap();
}

#[test]
fn invalid_room_create() {
	// With a prev event.
	let content = json!({
		"creator": alice(),
	});
	let event = to_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&content).unwrap(),
		&["OTHER_CREATE"],
		&["OTHER_CREATE"],
	);
	check_room_create(&RoomCreateEvent::new(event), &AuthorizationRules::V1).unwrap_err();

	// With an authorization event.
	let content = json!({ "creator": alice() });
	let event = to_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&content).unwrap(),
		&["MESSAGE"],
		&[],
	);

	check_room_create(&RoomCreateEvent::new(event), &AuthorizationRules::V1).unwrap_err();

	// Sender with a different domain.
	let creator = user_id!("@bot:bar");
	let content = json!({
		"creator": creator,
	});
	let event = to_init_pdu_event(
		"CREATE",
		creator,
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&content).unwrap(),
	);
	check_room_create(&RoomCreateEvent::new(event), &AuthorizationRules::V1).unwrap_err();

	// No creator in v1.
	let content = json!({});
	let event = to_init_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&content).unwrap(),
	);
	check_room_create(&RoomCreateEvent::new(event), &AuthorizationRules::V1).unwrap_err();
}

#[test]
fn redact_higher_power_level() {
	let _guard = init_subscriber();

	let incoming_event = room_redaction_pdu_event(
		"HELLO",
		charlie(),
		owned_event_id!("$redacted_event:other.server"),
		to_raw_json_value(&RoomRedactionEventContent::new_v1()).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let room_power_levels_event = Some(default_room_power_levels());

	// Cannot redact if redact level is higher than user's.
	check_room_redaction(
		&incoming_event,
		room_power_levels_event.as_ref(),
		&AuthorizationRules::V1,
		int!(0).into(),
	)
	.unwrap_err();
}

#[test]
fn redact_same_power_level() {
	let _guard = init_subscriber();

	let incoming_event = room_redaction_pdu_event(
		"HELLO",
		charlie(),
		owned_event_id!("$redacted_event:other.server"),
		to_raw_json_value(&RoomRedactionEventContent::new_v1()).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let room_power_levels_event = Some(RoomPowerLevelsEvent::new(to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&json!({ "users": { alice(): 100, charlie(): 50 } })).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	)));

	// Can redact if redact level is same as user's.
	check_room_redaction(
		&incoming_event,
		room_power_levels_event.as_ref(),
		&AuthorizationRules::V1,
		int!(50).into(),
	)
	.unwrap();
}

#[test]
fn redact_same_server() {
	let _guard = init_subscriber();

	let incoming_event = room_redaction_pdu_event(
		"HELLO",
		charlie(),
		event_id("redacted_event"),
		to_raw_json_value(&RoomRedactionEventContent::new_v1()).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let room_power_levels_event = Some(default_room_power_levels());

	// Can redact if redact level is same as user's.
	check_room_redaction(
		&incoming_event,
		room_power_levels_event.as_ref(),
		&AuthorizationRules::V1,
		int!(0).into(),
	)
	.unwrap();
}

#[tokio::test]
async fn missing_room_create_in_state() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	init_events.remove(&event_id("CREATE"));

	// Cannot accept event if no `m.room.create` in state.
	check_state_independent_auth_rules(
		&RoomVersionRules::V6,
		&incoming_event,
		&async |event_id| {
			init_events
				.get(&event_id)
				.cloned()
				.ok_or_else(not_found)
		},
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn reject_missing_room_create_auth_events() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["IMA", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();

	// Cannot accept event if no `m.room.create` in auth events.
	check_state_independent_auth_rules(
		&RoomVersionRules::V6,
		&incoming_event,
		&async |event_id| {
			init_events
				.get(&event_id)
				.cloned()
				.ok_or_else(not_found)
		},
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn no_federate_different_server() {
	let _guard = init_subscriber();

	let sender = user_id!("@aya:other.server");
	let incoming_event = to_pdu_event(
		"AYA_JOIN",
		sender,
		TimelineEventType::RoomMember,
		Some(sender.as_str()),
		member_content_join(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("CREATE")).unwrap() = to_init_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&json!({
			"creator": alice(),
			"m.federate": false,
		}))
		.unwrap(),
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	// Cannot accept event if not federating and different server.
	check_state_dependent_auth_rules(&RoomVersionRules::V6, &incoming_event, &fetch_state)
		.await
		.unwrap_err();
}

#[tokio::test]
async fn no_federate_same_server() {
	let _guard = init_subscriber();

	let sender = user_id!("@aya:foo");
	let incoming_event = to_pdu_event(
		"AYA_JOIN",
		sender,
		TimelineEventType::RoomMember,
		Some(sender.as_str()),
		member_content_join(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("CREATE")).unwrap() = to_init_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&json!({
			"creator": alice(),
			"m.federate": false,
		}))
		.unwrap(),
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	// Accept event if not federating and same server.
	check_state_dependent_auth_rules(&RoomVersionRules::V6, &incoming_event, &fetch_state)
		.await
		.unwrap();
}

// `m.room.aliases` event type and content removed from ruma upstream;
// pre-v6 alias auth-rule tests are no longer expressible.

#[tokio::test]
async fn sender_not_in_room() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["IMA", "IPOWER", "CREATE"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	// Cannot accept event if user not in room.
	check_state_dependent_auth_rules(&RoomVersionRules::V6, &incoming_event, &fetch_state)
		.await
		.unwrap_err();
}

#[tokio::test]
async fn room_third_party_invite_not_enough_power() {
	let _guard = init_subscriber();

	let incoming_event = room_third_party_invite(charlie());

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IPOWER")).unwrap() = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&json!({
			"users": { alice(): 100 },
			"invite": 50,
		}))
		.unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	// Cannot accept `m.room.third_party_invite` if not enough power.
	check_state_dependent_auth_rules(&RoomVersionRules::V6, &incoming_event, &fetch_state)
		.await
		.unwrap_err();
}

#[tokio::test]
async fn room_third_party_invite_with_enough_power() {
	let _guard = init_subscriber();

	let incoming_event = room_third_party_invite(charlie());

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	// Accept `m.room.third_party_invite` if enough power.
	check_state_dependent_auth_rules(&RoomVersionRules::V6, &incoming_event, &fetch_state)
		.await
		.unwrap();
}

#[tokio::test]
async fn event_type_not_enough_power() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IPOWER")).unwrap() = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&json!({
			"users": { alice(): 100 },
			"events": {
				"m.room.message": "50",
			},
		}))
		.unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	// Cannot send event if not enough power for the event's type.
	check_state_dependent_auth_rules(&RoomVersionRules::V6, &incoming_event, &fetch_state)
		.await
		.unwrap_err();
}

#[tokio::test]
async fn user_id_state_key_not_sender() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		"dev.ruma.fake_state_event".into(),
		Some(ella().as_str()),
		to_raw_json_value(&json!({})).unwrap(),
		&["IMA", "IPOWER", "CREATE"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	// Cannot send state event with a user ID as a state key that doesn't match the
	// sender.
	check_state_dependent_auth_rules(&RoomVersionRules::V6, &incoming_event, &fetch_state)
		.await
		.unwrap_err();
}

#[tokio::test]
async fn user_id_state_key_is_sender() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		"dev.ruma.fake_state_event".into(),
		Some(alice().as_str()),
		to_raw_json_value(&json!({})).unwrap(),
		&["IMA", "IPOWER", "CREATE"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	// Can send state event with a user ID as a state key that matches the sender.
	check_state_dependent_auth_rules(&RoomVersionRules::V6, &incoming_event, &fetch_state)
		.await
		.unwrap();
}

#[tokio::test]
async fn auth_event_in_different_room() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	let power_level = PduEvent {
		event_id: event_id("IPOWER"),
		room_id: owned_room_id!("!wrongroom:foo"),
		sender: alice().to_owned(),
		origin: None,
		origin_server_ts: uint!(3),
		state_key: Some(StateKey::new()),
		kind: TimelineEventType::RoomPowerLevels,
		content: json!({ "users": { alice(): 100 } }).into(),
		redacts: None,
		unsigned: None,
		auth_events: vec![event_id("CREATE"), event_id("IMA")].into(),
		prev_events: vec![event_id("IMA")].into(),
		depth: uint!(0),
		hashes: EventHash::default(),
		//rejected: false,
	};
	init_events
		.insert(power_level.event_id.clone(), power_level)
		.unwrap();

	// Cannot accept with auth event in different room.
	check_state_independent_auth_rules(
		&RoomVersionRules::V6,
		&incoming_event,
		&async |event_id| {
			init_events
				.get(&event_id)
				.cloned()
				.ok_or_else(not_found)
		},
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn duplicate_auth_event_type() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["CREATE", "IMA", "IMA2", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	init_events.insert(
		event_id("IMA2"),
		to_pdu_event(
			"IMA2",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			&["CREATE", "IMA"],
			&["IMA"],
		),
	);

	// Cannot accept with two auth events with same (type, state_key) pair.
	check_state_independent_auth_rules(
		&RoomVersionRules::V6,
		&incoming_event,
		&async |event_id| {
			init_events
				.get(&event_id)
				.cloned()
				.ok_or_else(not_found)
		},
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn unexpected_auth_event_type() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["CREATE", "IMA", "IPOWER", "IMC"],
		&["IMC"],
	);

	let mut init_events = INITIAL_EVENTS();
	init_events.insert(
		event_id("IMC"),
		to_pdu_event(
			"IMC",
			charlie(),
			TimelineEventType::RoomMember,
			Some(charlie().as_str()),
			member_content_join(),
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		),
	);

	// Cannot accept with auth event with unexpected (type, state_key) pair.
	check_state_independent_auth_rules(
		&RoomVersionRules::V6,
		&incoming_event,
		&async |event_id| {
			init_events
				.get(&event_id)
				.cloned()
				.ok_or_else(not_found)
		},
	)
	.await
	.unwrap_err();
}

#[tokio::test]
#[ignore = "PduEvent::rejected not conditionally compiled here"]
async fn rejected_auth_event() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	let power_level = PduEvent {
		event_id: event_id("IPOWER"),
		room_id: room_id().to_owned(),
		sender: alice().to_owned(),
		origin: None,
		origin_server_ts: uint!(3),
		state_key: Some(StateKey::new()),
		kind: TimelineEventType::RoomPowerLevels,
		content: json!({ "users": { alice(): 100 } }).into(),
		redacts: None,
		unsigned: None,
		auth_events: vec![event_id("CREATE"), event_id("IMA")].into(),
		prev_events: vec![event_id("IMA")].into(),
		depth: uint!(0),
		hashes: EventHash::default(),
		//rejected: true,
	};
	init_events
		.insert(power_level.event_id.clone(), power_level)
		.unwrap();

	// Cannot accept with auth event that was rejected.
	check_state_independent_auth_rules(
		&RoomVersionRules::V6,
		&incoming_event,
		&async |event_id| {
			init_events
				.get(&event_id)
				.cloned()
				.ok_or_else(not_found)
		},
	)
	.await
	.unwrap_err();
}

#[test]
fn room_create_with_allowed_or_rejected_room_id() {
	// v11, room_id is required.
	let v11_content = json!({
		"room_version": "11",
	});

	let event_with_room_id = to_init_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&v11_content).unwrap(),
	);
	check_room_create(&RoomCreateEvent::new(event_with_room_id), &AuthorizationRules::V11)
		.unwrap();

	let event_no_room_id =
		room_create_hydra_pdu_event("CREATE", alice(), to_raw_json_value(&v11_content).unwrap());
	check_room_create(&RoomCreateEvent::new(event_no_room_id), &AuthorizationRules::V11)
		.unwrap_err();

	// `org.matrix.hydra.11`, room_id is rejected.
	let hydra_content = json!({
		"room_version": "12",
	});

	let event_with_room_id = to_init_pdu_event(
		"CREATE",
		alice(),
		TimelineEventType::RoomCreate,
		Some(""),
		to_raw_json_value(&hydra_content).unwrap(),
	);
	check_room_create(&RoomCreateEvent::new(event_with_room_id), &AuthorizationRules::V12)
		.unwrap_err();

	let event_no_room_id = room_create_hydra_pdu_event(
		"CREATE",
		alice(),
		to_raw_json_value(&hydra_content).unwrap(),
	);
	check_room_create(&RoomCreateEvent::new(event_no_room_id), &AuthorizationRules::V12).unwrap();
}

#[tokio::test]
async fn event_without_room_id() {
	let _guard = init_subscriber();

	let incoming_event = PduEvent {
		event_id: owned_event_id!("$HELLO"),
		room_id: owned_room_id!("!IGNORED"),
		sender: alice().to_owned(),
		origin: None,
		origin_server_ts: uint!(3),
		state_key: None,
		kind: TimelineEventType::RoomMessage,
		content: to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!"))
			.unwrap()
			.into(),
		redacts: None,
		unsigned: None,
		auth_events: [
			owned_event_id!("$CREATE"),
			owned_event_id!("$IMA"),
			owned_event_id!("$IPOWER"),
		]
		.into(),
		prev_events: [owned_event_id!("$IPOWER")].into(),
		depth: uint!(0),
		hashes: EventHash::default(),
		//rejected: false,
	};

	let init_events = INITIAL_HYDRA_EVENTS();

	// Cannot accept event without room ID.
	check_state_independent_auth_rules(
		&RoomVersionRules::V11,
		&incoming_event,
		&async |event_id| {
			init_events
				.get(&event_id)
				.cloned()
				.ok_or_else(not_found)
		},
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn allow_missing_room_create_auth_events() {
	let _guard = init_subscriber();

	let incoming_event = to_hydra_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["IMA", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_HYDRA_EVENTS();

	// Accept event if no `m.room.create` in auth events.
	check_state_independent_auth_rules(
		&RoomVersionRules::V12,
		&incoming_event,
		&async |event_id| {
			init_events
				.get(&event_id)
				.cloned()
				.ok_or_else(not_found)
		},
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn reject_room_create_in_auth_events() {
	let _guard = init_subscriber();

	let incoming_event = to_hydra_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_HYDRA_EVENTS();

	// Reject event if `m.room.create` in auth events.
	check_state_independent_auth_rules(
		&RoomVersionRules::V12,
		&incoming_event,
		&async |event_id| {
			init_events
				.get(&event_id)
				.cloned()
				.ok_or_else(not_found)
		},
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn missing_room_create_in_fetch_event() {
	let _guard = init_subscriber();

	let incoming_event = to_hydra_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_HYDRA_EVENTS();
	init_events
		.remove(&owned_event_id!("$CREATE"))
		.unwrap();

	// Reject event if `m.room.create` can't be found.
	check_state_independent_auth_rules(
		&RoomVersionRules::V12,
		&incoming_event,
		&async |event_id| {
			init_events
				.get(&event_id)
				.cloned()
				.ok_or_else(not_found)
		},
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn v12_additional_creator_cannot_bootstrap_join() {
	let _guard = init_subscriber();

	let create_content = json!({
		"room_version": "12",
		"additional_creators": ["@charlie:foo"],
	});

	let create = room_create_hydra_pdu_event(
		"CREATE",
		alice(),
		to_raw_json_value(&create_content).unwrap(),
	);

	let charlie_join = to_hydra_pdu_event::<&str>(
		"CHARLIE_JOIN",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_join(),
		&[],
		&["CREATE"],
	);

	let mut init_events: std::collections::HashMap<ruma::OwnedEventId, PduEvent> =
		std::collections::HashMap::new();
	init_events.insert(create.event_id().to_owned(), create);
	init_events.insert(charlie_join.event_id().to_owned(), charlie_join.clone());

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	check_state_dependent_auth_rules(&RoomVersionRules::V12, &charlie_join, &fetch_state)
		.await
		.unwrap_err();
}

#[tokio::test]
async fn v12_create_sender_can_bootstrap_join() {
	let _guard = init_subscriber();

	let create_content = json!({
		"room_version": "12",
	});

	let create = room_create_hydra_pdu_event(
		"CREATE",
		alice(),
		to_raw_json_value(&create_content).unwrap(),
	);

	let alice_join = to_hydra_pdu_event::<&str>(
		"ALICE_JOIN",
		alice(),
		TimelineEventType::RoomMember,
		Some(alice().as_str()),
		member_content_join(),
		&[],
		&["CREATE"],
	);

	let mut init_events: std::collections::HashMap<ruma::OwnedEventId, PduEvent> =
		std::collections::HashMap::new();
	init_events.insert(create.event_id().to_owned(), create);
	init_events.insert(alice_join.event_id().to_owned(), alice_join.clone());

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	check_state_dependent_auth_rules(&RoomVersionRules::V12, &alice_join, &fetch_state)
		.await
		.unwrap();
}

#[tokio::test]
#[ignore = "PduEvent::rejected not conditionally compiled here"]
async fn rejected_room_create_in_fetch_event() {
	let _guard = init_subscriber();

	let incoming_event = to_hydra_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMessage,
		None,
		to_raw_json_value(&RoomMessageEventContent::text_plain("Hi!")).unwrap(),
		&["IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_HYDRA_EVENTS();
	let create_event_id = owned_event_id!("$CREATE");
	let create_event = init_events.remove(&create_event_id).unwrap();
	//create_event.rejected = true;
	init_events.insert(create_event_id, create_event);

	// Reject event if `m.room.create` was rejected.
	check_state_independent_auth_rules(
		&RoomVersionRules::V12,
		&incoming_event,
		&async |event_id| {
			init_events
				.get(&event_id)
				.cloned()
				.ok_or_else(not_found)
		},
	)
	.await
	.unwrap_err();
}

// `m.room.member` knock predicate. v7-v9: only `knock` join_rule accepts.
// v10+: `knock` or `knock_restricted` accepts.

fn member_content_knock() -> Box<serde_json::value::RawValue> {
	to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap()
}

fn knock_test_events(
	join_rule: JoinRule,
) -> std::collections::HashMap<ruma::OwnedEventId, PduEvent> {
	let mut init_events = INITIAL_EVENTS();

	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(join_rule)).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	init_events.insert(
		event_id("ZARA_LEAVE"),
		to_pdu_event(
			"ZARA_LEAVE",
			zara(),
			TimelineEventType::RoomMember,
			Some(zara().as_str()),
			to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
			&["CREATE", "IJR", "IPOWER"],
			&["IJR"],
		),
	);

	init_events
}

fn zara_knock_event() -> PduEvent {
	to_pdu_event(
		"ZARA_KNOCK",
		zara(),
		TimelineEventType::RoomMember,
		Some(zara().as_str()),
		member_content_knock(),
		&["CREATE", "IJR", "IPOWER"],
		&["ZARA_LEAVE"],
	)
}

#[tokio::test]
async fn knock_with_public_join_rule_rejected_v7() {
	let _guard = init_subscriber();

	let init_events = knock_test_events(JoinRule::Public);
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	check_state_dependent_auth_rules(&RoomVersionRules::V7, &zara_knock_event(), &fetch_state)
		.await
		.unwrap_err();
}

#[tokio::test]
async fn knock_with_invite_join_rule_rejected_v8() {
	let _guard = init_subscriber();

	let init_events = knock_test_events(JoinRule::Invite);
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	check_state_dependent_auth_rules(&RoomVersionRules::V8, &zara_knock_event(), &fetch_state)
		.await
		.unwrap_err();
}

#[tokio::test]
async fn knock_with_knock_join_rule_accepted_v7() {
	let _guard = init_subscriber();

	let init_events = knock_test_events(JoinRule::Knock);
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	check_state_dependent_auth_rules(&RoomVersionRules::V7, &zara_knock_event(), &fetch_state)
		.await
		.unwrap();
}

#[tokio::test]
async fn knock_with_public_join_rule_rejected_v10() {
	let _guard = init_subscriber();

	let init_events = knock_test_events(JoinRule::Public);
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	check_state_dependent_auth_rules(&RoomVersionRules::V10, &zara_knock_event(), &fetch_state)
		.await
		.unwrap_err();
}

#[tokio::test]
async fn knock_with_knock_restricted_join_rule_accepted_v10() {
	let _guard = init_subscriber();

	let init_events = knock_test_events(JoinRule::KnockRestricted(Restricted::new(vec![])));
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	check_state_dependent_auth_rules(&RoomVersionRules::V10, &zara_knock_event(), &fetch_state)
		.await
		.unwrap();
}

#[tokio::test]
async fn knock_with_knock_restricted_join_rule_rejected_v8() {
	let _guard = init_subscriber();

	// knock_restricted does not exist before v10; in v8 the only accepted
	// value is `knock`.
	let init_events = knock_test_events(JoinRule::KnockRestricted(Restricted::new(vec![])));
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();

	check_state_dependent_auth_rules(&RoomVersionRules::V8, &zara_knock_event(), &fetch_state)
		.await
		.unwrap_err();
}
