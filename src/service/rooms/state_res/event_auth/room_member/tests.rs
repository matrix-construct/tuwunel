use ruma::{
	Signatures,
	events::{
		TimelineEventType,
		room::{
			join_rules::{JoinRule, Restricted, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent, SignedContent, ThirdPartyInvite},
			third_party_invite::RoomThirdPartyInviteEventContent,
		},
	},
	room_version_rules::AuthorizationRules,
	serde::Raw,
	third_party_invite::IdentityServerBase64PublicKey,
};
use serde_json::{json, value::to_raw_value as to_raw_json_value};

use super::{
	super::events::RoomMemberEvent,
	check_room_member,
	test_utils::{
		INITIAL_EVENTS, INITIAL_EVENTS_CREATE_ROOM, TestStateMap, alice, bob, charlie, ella,
		event_id, init_subscriber, member_content_ban, member_content_join,
		room_third_party_invite, to_pdu_event, zara,
	},
};

#[tokio::test]
async fn missing_state_key() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		None,
		member_content_join(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Event should have a state key.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn missing_membership() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&json!({})).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Content should at least include `membership`.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn join_after_create_creator_match() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMember,
		Some(alice().as_str()),
		member_content_join(),
		&["CREATE"],
		&["CREATE"],
	);

	let init_events = INITIAL_EVENTS_CREATE_ROOM();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Before v11, the `creator` of `m.room.create` must be the same as the state
	// key.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn join_after_create_creator_mismatch() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_join(),
		&["CREATE"],
		&["CREATE"],
	);

	let init_events = INITIAL_EVENTS_CREATE_ROOM();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Before v11, the `creator` of `m.room.create` must be the same as the state
	// key.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn join_after_create_sender_match() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMember,
		Some(alice().as_str()),
		member_content_join(),
		&["CREATE"],
		&["CREATE"],
	);

	let init_events = INITIAL_EVENTS_CREATE_ROOM();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Since v11, the `sender` of `m.room.create` must be the same as the state key.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V11,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn join_after_create_sender_mismatch() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_join(),
		&["CREATE"],
		&["CREATE"],
	);

	let init_events = INITIAL_EVENTS_CREATE_ROOM();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Since v11, the `sender` of `m.room.create` must be the same as the state key.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V11,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn join_sender_state_key_mismatch() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(alice().as_str()),
		member_content_join(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// For join events, the sender must be the same as the state key.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn join_banned() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_join(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IMC")).unwrap() = to_pdu_event(
		"IMC",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_ban(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// A user cannot join if they are banned.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn join_invite_join_rule_already_joined() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_join(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Invite)).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// A user can send a join event in a room with `invite` join rule if they
	// already joined.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn join_knock_join_rule_already_invited() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_join(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IMC")).unwrap() = to_pdu_event(
		"IMC",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Invite)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Knock)).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Since v7, a user can send a join event in a room with `knock` join rule if
	// they are were invited.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V7,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn join_knock_join_rule_not_supported() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_join(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Knock)).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Before v7, a user CANNOT send a join event in a room with `knock` join rule.
	// Servers should not allow that join rule if it's not supported by the room
	// version, but this is good for coverage.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn join_restricted_join_rule_not_supported() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_join(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Restricted(
			Restricted::new(vec![]),
		)))
		.unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Before v8, a user CANNOT send a join event in a room with `restricted` join
	// rule. Servers should not allow that join rule if it's not supported by the
	// room version, but this is good for coverage.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn join_knock_restricted_join_rule_not_supported() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_join(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::KnockRestricted(
			Restricted::new(vec![]),
		)))
		.unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Before v10, a user CANNOT send a join event in a room with `knock_restricted`
	// join rule. Servers should not allow that join rule if it's not supported by
	// the room version, but this is good for coverage.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn join_restricted_join_rule_already_joined() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_join(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Restricted(
			Restricted::new(vec![]),
		)))
		.unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Since v8, a user can send a join event in a room with `restricted` join rule
	// if they already joined.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn join_knock_restricted_join_rule_already_invited() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_join(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IMC")).unwrap() = to_pdu_event(
		"IMC",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Invite)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::KnockRestricted(
			Restricted::new(vec![]),
		)))
		.unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Since v10, a user can send a join event in a room with `knock_restricted`
	// join rule if they were invited.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V10,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn join_restricted_join_rule_missing_join_authorised_via_users_server() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		member_content_join(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Restricted(
			Restricted::new(vec![]),
		)))
		.unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Since v8, a user CANNOT join event in a room with `restricted` join rule if
	// there is no `join_authorised_via_users_server` property.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn join_restricted_join_rule_authorised_via_user_not_in_room() {
	let _guard = init_subscriber();

	let mut content = RoomMemberEventContent::new(MembershipState::Join);
	content.join_authorized_via_users_server = Some(zara().to_owned());

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Restricted(
			Restricted::new(vec![]),
		)))
		.unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Since v8, a user CANNOT join event in a room with `restricted` join rule if
	// they were authorized by a user not in the room.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn join_restricted_join_rule_authorised_via_user_with_not_enough_power() {
	let _guard = init_subscriber();

	let mut content = RoomMemberEventContent::new(MembershipState::Join);
	content.join_authorized_via_users_server = Some(charlie().to_owned());

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IPOWER")).unwrap() = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&json!({ "users": { alice(): 100 }, "invite": 50 })).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Restricted(
			Restricted::new(vec![]),
		)))
		.unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Since v8, a user CANNOT join event in a room with `restricted` join rule if
	// they were authorized by a user with not enough power.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn join_restricted_join_rule_authorised_via_user() {
	let _guard = init_subscriber();

	// Check various contents that might not match the definition of
	// `m.room.join_rules` in the spec, to ensure that we only care about the
	// `join_rule` field.
	let join_rules_to_check = [
		// Valid content, but we don't care about the allow rules.
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Restricted(
			Restricted::new(vec![]),
		)))
		.unwrap(),
		// Invalid room ID, real-life example from <https://github.com/ruma/ruma/issues/1867>.
		to_raw_json_value(&json!({
			"allow": [
				{
					"room_id": "",
					"type": "m.room_membership",
				},
			],
			"join_rule": "restricted",
		}))
		.unwrap(),
		// Missing room ID.
		to_raw_json_value(&json!({
			"allow": [
				{
					"type": "m.room_membership",
				},
			],
			"join_rule": "restricted",
		}))
		.unwrap(),
	];

	let mut content = RoomMemberEventContent::new(MembershipState::Join);
	content.join_authorized_via_users_server = Some(charlie().to_owned());

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();

	for join_rule_content in join_rules_to_check {
		*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
			"IJR",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			join_rule_content,
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		);

		let auth_events = TestStateMap::new(&init_events);
		let fetch_state = auth_events.fetch_state_fn();
		let room_create_event = auth_events.room_create_event();

		// Since v8, a user can join event in a room with `restricted` join rule if they
		// were authorized by a user with enough power.
		check_room_member(
			&RoomMemberEvent::new(incoming_event.clone()),
			&AuthorizationRules::V8,
			&room_create_event,
			&fetch_state,
		)
		.await
		.unwrap();
	}
}

#[tokio::test]
async fn join_public_join_rule() {
	let _guard = init_subscriber();

	// Check various contents that might not match the definition of `m.room.member`
	// in the spec, to ensure that we only care about a few fields.
	let contents_to_check = [
		// Valid content.
		member_content_join(),
		// Invalid displayname.
		to_raw_json_value(&json!({
			"membership": "join",
			"displayname": 203,
		}))
		.unwrap(),
		// Invalid is_direct.
		to_raw_json_value(&json!({
			"membership": "join",
			"is_direct": "yes",
		}))
		.unwrap(),
	];

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	for content in contents_to_check {
		let incoming_event = to_pdu_event(
			"HELLO",
			ella(),
			TimelineEventType::RoomMember,
			Some(ella().as_str()),
			content,
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		);

		// A user can join a room with a `public` join rule.
		check_room_member(
			&RoomMemberEvent::new(incoming_event),
			&AuthorizationRules::V8,
			&room_create_event,
			&fetch_state,
		)
		.await
		.unwrap();
	}
}

#[tokio::test]
async fn invite_via_third_party_invite_banned() {
	let _guard = init_subscriber();

	let mut content = RoomMemberEventContent::new(MembershipState::Invite);
	content.third_party_invite = Some(ThirdPartyInvite::new(
		"e..@p..".to_owned(),
		Raw::new(&SignedContent::new(
			Signatures::new(),
			ella().to_owned(),
			"somerandomtoken".to_owned(),
		))
		.unwrap(),
	));

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "BAN", "IPOWER"],
		&["BAN"],
	);

	let mut init_events = INITIAL_EVENTS();
	init_events.insert(
		event_id("BAN"),
		to_pdu_event(
			"BAN",
			alice(),
			TimelineEventType::RoomMember,
			Some(ella().as_str()),
			member_content_ban(),
			&["CREATE", "IJR", "IPOWER"],
			&["IPOWER"],
		),
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// A user cannot be invited via third party invite if they were banned.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_via_third_party_invite_missing_signed() {
	let _guard = init_subscriber();

	let content = json!({
		"membership": "invite",
		"third_party_invite": {
			"display_name": "e..@p..",
		},
	});

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Third party invite content must have a `joined` property.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_via_third_party_invite_missing_mxid() {
	let _guard = init_subscriber();

	let content = json!({
		"membership": "invite",
		"third_party_invite": {
			"display_name": "e..@p..",
			"signed": {
				"token": "somerandomtoken",
			},
		},
	});

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Third party invite content must have a `joined.mxid` property.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_via_third_party_invite_missing_token() {
	let _guard = init_subscriber();

	let content = json!({
		"membership": "invite",
		"third_party_invite": {
			"display_name": "e..@p..",
			"signed": {
				"mxid": ella(),
			},
		},
	});

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Third party invite content must have a `joined.token` property.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_via_third_party_invite_mxid_mismatch() {
	let _guard = init_subscriber();

	let mut content = RoomMemberEventContent::new(MembershipState::Invite);
	content.third_party_invite = Some(ThirdPartyInvite::new(
		"z..@p..".to_owned(),
		Raw::new(&SignedContent::new(
			Signatures::new(),
			zara().to_owned(),
			"somerandomtoken".to_owned(),
		))
		.unwrap(),
	));

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// `mxid` of third party invite must match state key.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_via_third_party_invite_missing_room_third_party_invite() {
	let _guard = init_subscriber();

	let mut content = RoomMemberEventContent::new(MembershipState::Invite);
	content.third_party_invite = Some(ThirdPartyInvite::new(
		"e..@p..".to_owned(),
		Raw::new(&SignedContent::new(
			Signatures::new(),
			ella().to_owned(),
			"somerandomtoken".to_owned(),
		))
		.unwrap(),
	));

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER", "THIRDPARTY"],
		&["THIRDPARTY"],
	);

	let mut init_events = INITIAL_EVENTS();
	init_events.insert(
		event_id("THIRD_PARTY"),
		to_pdu_event(
			"THIRDPARTY",
			charlie(),
			TimelineEventType::RoomThirdPartyInvite,
			Some("wrong_token"),
			to_raw_json_value(&RoomThirdPartyInviteEventContent::new(
				"e..@p..".to_owned(),
				"http://host.local/check/public_key".to_owned(),
				IdentityServerBase64PublicKey::new(b"public_key"),
			))
			.unwrap(),
			&["CREATE", "IJR", "IPOWER"],
			&["IPOWER"],
		),
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// There must be an `m.room.third_party_invite` event with the same token in the
	// state.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_via_third_party_invite_room_third_party_invite_sender_mismatch() {
	let _guard = init_subscriber();

	let mut content = RoomMemberEventContent::new(MembershipState::Invite);
	content.third_party_invite = Some(ThirdPartyInvite::new(
		"e..@p..".to_owned(),
		Raw::new(&SignedContent::new(
			Signatures::new(),
			ella().to_owned(),
			"somerandomtoken".to_owned(),
		))
		.unwrap(),
	));

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	init_events.insert(event_id("THIRD_PARTY"), room_third_party_invite(bob()));

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// `mxid` of third party invite must match state key.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_via_third_party_invite_with_room_missing_signatures() {
	let _guard = init_subscriber();

	let content = json!({
		"membership": "invite",
		"third_party_invite": {
			"display_name": "e...@p...",
			"signed": {
				"mxid": "@ella:foo",
				"sender": "@charlie:foo",
				"token": "somerandomtoken",
			}
		}
	});

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER", "THIRDPARTY"],
		&["THIRDPARTY"],
	);

	let mut init_events = INITIAL_EVENTS();
	init_events.insert(event_id("THIRD_PARTY"), room_third_party_invite(charlie()));

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// `signed` must have a `signatures` field.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_via_third_party_invite_with_room_empty_signatures() {
	let _guard = init_subscriber();

	let content = json!({
		"membership": "invite",
		"third_party_invite": {
			"display_name": "e...@p...",
			"signed": {
				"mxid": "@ella:foo",
				"sender": "@charlie:foo",
				"token": "somerandomtoken",
				"signatures": [],
			}
		}
	});

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER", "THIRDPARTY"],
		&["THIRDPARTY"],
	);

	let mut init_events = INITIAL_EVENTS();
	init_events.insert(event_id("THIRD_PARTY"), room_third_party_invite(charlie()));

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// There is no signature to verify, we need at least one.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_via_third_party_invite_with_wrong_signature() {
	let _guard = init_subscriber();

	let content = json!({
		"membership": "invite",
		"third_party_invite": {
			"display_name": "e...@p...",
			"signed": {
				"mxid": "@ella:foo",
				"sender": "@charlie:foo",
				"token": "somerandomtoken",
				"signatures": {
					"identity.local": {
						"ed25519:0": "ClearlyWrongSignature",
					}
				},
			}
		}
	});

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER", "THIRDPARTY"],
		&["THIRDPARTY"],
	);

	let mut init_events = INITIAL_EVENTS();
	init_events.insert(event_id("THIRD_PARTY"), room_third_party_invite(charlie()));

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// No public key will manage to verify the signature.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_via_third_party_invite_with_wrong_signing_algorithm() {
	let _guard = init_subscriber();

	let content = json!({
		"membership": "invite",
		"third_party_invite": {
			"display_name": "e...@p...",
			"signed": {
				"mxid": "@ella:foo",
				"sender": "@charlie:foo",
				"token": "somerandomtoken",
				"signatures": {
					"identity.local": {
						"unknown:0": "EyW7uaJagmhIQg7DUFpiJv9ur8h8DkDSjyV6f5MlROJrrkg8JElBFKr2iTQY9x+A6OauQdNy7L9T4xgzIZVbCA"
					}
				},
			}
		}
	});

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER", "THIRDPARTY"],
		&["THIRDPARTY"],
	);

	let mut init_events = INITIAL_EVENTS();
	init_events.insert(event_id("THIRD_PARTY"), room_third_party_invite(charlie()));

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Can't verify a signature with an unsupported algorithm, so there is no
	// signature to verify.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_via_third_party_invite() {
	let _guard = init_subscriber();

	let content = json!({
		"membership": "invite",
		"third_party_invite": {
			"display_name": "e...@p...",
			"signed": {
				"mxid": "@ella:foo",
				"sender": "@charlie:foo",
				"token": "somerandomtoken",
				"signatures": {
					"identity.local": {
						// This signature will be ignored because the algorithm is unsupported.
						"unknown:0": "SomeSignature",
						// This signature will fail the verification.
						"ed25519:0": "ClearlyWrongSignature",
						// This signature will pass verification!
						"ed25519:1": "EyW7uaJagmhIQg7DUFpiJv9ur8h8DkDSjyV6f5MlROJrrkg8JElBFKr2iTQY9x+A6OauQdNy7L9T4xgzIZVbCA"
					}
				},
			}
		}
	});

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IJR", "IPOWER", "THIRDPARTY"],
		&["THIRDPARTY"],
	);

	let mut init_events = INITIAL_EVENTS();
	init_events.insert(event_id("THIRD_PARTY"), room_third_party_invite(charlie()));

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Valid third party invite works.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn invite_sender_not_joined() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		zara(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Invite)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// The sender of the invite must have joined the room.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_banned() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Invite)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IMC")).unwrap() = to_pdu_event(
		"IMC",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_ban(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// The sender of the invite must have joined the room.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_already_joined() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Invite)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// The sender of the invite must have joined the room.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite_sender_not_enough_power() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Invite)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IPOWER")).unwrap() = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&json!({ "users": { alice(): 100 }, "invite": 50 })).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// The sender must have enough power to invite in the room.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn invite() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Invite)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// The invite is valid.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn leave_after_leave() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can only leave after `invite`, `join` or `knock`.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn leave_after_join() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can leave after join.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn leave_after_invite() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IMC")).unwrap() = to_pdu_event(
		"IMC",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Invite)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can leave after invite.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn leave_after_knock() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IMC")).unwrap() = to_pdu_event(
		"IMC",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can leave after knock.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V8,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn leave_after_knock_not_supported() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IMC")).unwrap() = to_pdu_event(
		"IMC",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can't leave if the room version does not support knocking. Servers
	// should not allow that membership if it's not supported by the room version,
	// but this is good for coverage.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn leave_kick_sender_left() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		zara(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can't kick if not joined.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn leave_unban_not_enough_power() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		bob(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IMC")).unwrap() = to_pdu_event(
		"IMC",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_ban(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can't unban if not enough power.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn leave_unban() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IMC")).unwrap() = to_pdu_event(
		"IMC",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_ban(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can unban with enough power.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn leave_kick_not_enough_power() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		bob(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can't kick if not enough power for it.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn leave_kick_greater_power() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		bob(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IPOWER")).unwrap() = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&json!({
			"users": {
				alice(): 100,
				bob(): 50,
				charlie(): 60,
			},
		}))
		.unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Can't kick user with greater power level.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn leave_kick_same_power() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		bob(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IPOWER")).unwrap() = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&json!({
			"users": {
				alice(): 100,
				bob(): 50,
				charlie(): 50,
			},
		}))
		.unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Can't kick user with same power level.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn leave_kick() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Leave)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Can kick user with enough power.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn ban_sender_not_joined() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_ban(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Can't ban user if not in room.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn ban_not_enough_power() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(bob().as_str()),
		member_content_ban(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Can't ban user if not enough power.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn ban_greater_power() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(bob().as_str()),
		member_content_ban(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IPOWER")).unwrap() = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&json!({
			"users": {
				alice(): 100,
				bob(): 60,
				charlie(): 50,
			},
		}))
		.unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Can't ban user with greater power level.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn ban_same_power() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(bob().as_str()),
		member_content_ban(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IPOWER")).unwrap() = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&json!({
			"users": {
				alice(): 100,
				bob(): 50,
				charlie(): 50,
			},
		}))
		.unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Can't ban user with same power level.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn ban() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		alice(),
		TimelineEventType::RoomMember,
		Some(bob().as_str()),
		member_content_ban(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// Can ban user with enough power.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V6,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn knock_public_join_rule() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let init_events = INITIAL_EVENTS();
	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can't knock if join rule is not `knock` or `knock_restricted`.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V11,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn knock_knock_join_rule() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Knock)).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can knock if room version supports it.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V7,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn knock_knock_join_rule_not_supported() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Knock)).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User CANNOT knock if room version doesn't support it.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V3,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn knock_knock_restricted_join_rule() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::KnockRestricted(
			Restricted::new(vec![]),
		)))
		.unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User can knock if room version supports it.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V10,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap();
}

#[tokio::test]
async fn knock_knock_restricted_join_rule_not_supported() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		ella(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::KnockRestricted(
			Restricted::new(vec![]),
		)))
		.unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User CANNOT knock if room version doesn't support it.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V3,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn knock_sender_state_key_mismatch() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		zara(),
		TimelineEventType::RoomMember,
		Some(ella().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Knock)).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User cannot knock if state key doesn't match sender.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V7,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn knock_after_ban() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Knock)).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);
	*init_events.get_mut(&event_id("IMC")).unwrap() = to_pdu_event(
		"IMC",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		member_content_ban(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User cannot knock if banned.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V7,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn knock_after_invite() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Knock)).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);
	*init_events.get_mut(&event_id("IMC")).unwrap() = to_pdu_event(
		"IMC",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Invite)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IMB"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User cannot knock after being invited.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V7,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}

#[tokio::test]
async fn knock_after_join() {
	let _guard = init_subscriber();

	let incoming_event = to_pdu_event(
		"HELLO",
		charlie(),
		TimelineEventType::RoomMember,
		Some(charlie().as_str()),
		to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
		&["CREATE", "IJR", "IPOWER"],
		&["IPOWER"],
	);

	let mut init_events = INITIAL_EVENTS();
	*init_events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
		"IJR",
		alice(),
		TimelineEventType::RoomJoinRules,
		Some(""),
		to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Knock)).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let auth_events = TestStateMap::new(&init_events);
	let fetch_state = auth_events.fetch_state_fn();
	let room_create_event = auth_events.room_create_event();

	// User cannot knock after being invited.
	check_room_member(
		&RoomMemberEvent::new(incoming_event),
		&AuthorizationRules::V7,
		&room_create_event,
		&fetch_state,
	)
	.await
	.unwrap_err();
}
