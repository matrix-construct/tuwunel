use std::iter;

use ruma::{
	events::{TimelineEventType, room::power_levels::UserPowerLevel},
	int,
	room_version_rules::AuthorizationRules,
};
use serde_json::{
	Value as JsonValue, json,
	value::{Map as JsonMap, to_raw_value as to_raw_json_value},
};
use tuwunel_core::{extract_variant, info, matrix::PduEvent};

use super::super::{
	check_room_power_levels,
	events::RoomPowerLevelsEvent,
	test_utils::{alice, bob, init_subscriber, to_pdu_event, zara},
};

/// The default `m.room.power_levels` event when creating a public room.
pub(super) fn default_room_power_levels() -> RoomPowerLevelsEvent<PduEvent> {
	RoomPowerLevelsEvent::new(to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&json!({ "users": { alice(): 100 } })).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	))
}

#[test]
fn not_int_or_string_int_in_content() {
	let _guard = init_subscriber();

	let original_content = json!({
		"users": { alice(): 100 },
	});
	let original_content_object = extract_variant!(original_content, JsonValue::Object).unwrap();

	let current_room_power_levels_event = Some(default_room_power_levels());

	let int_fields = &[
		"users_default",
		"events_default",
		"state_default",
		"ban",
		"redact",
		"kick",
		"invite",
	];

	// Tuples of (is_string, is_int) booleans.
	let combinations = &[(true, false), (true, true), (false, true)];

	for field in int_fields {
		for (is_string, is_int) in combinations {
			info!(?field, ?is_string, ?is_int, "checking field");

			let value = match (is_string, is_int) {
				| (true, false) => "foo".into(),
				| (true, true) => "50".into(),
				| (false, true) => 50.into(),
				| _ => unreachable!(),
			};

			let mut content_object = original_content_object.clone();
			content_object.insert((*field).to_owned(), value);

			let incoming_event = to_pdu_event(
				"IPOWER2",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&content_object).unwrap(),
				&["CREATE", "IMA", "IPOWER"],
				&["IPOWER"],
			);

			// String that is not a number is not accepted.
			let v6_result = check_room_power_levels(
				&RoomPowerLevelsEvent::new(incoming_event.clone()),
				current_room_power_levels_event.as_ref(),
				&AuthorizationRules::V6,
				int!(100),
				iter::empty(),
			);

			if *is_int {
				v6_result.unwrap();
			} else {
				v6_result.unwrap_err();
			}

			// String is not accepted.
			let v10_result = check_room_power_levels(
				&RoomPowerLevelsEvent::new(incoming_event),
				current_room_power_levels_event.as_ref(),
				&AuthorizationRules::V10,
				int!(100),
				iter::empty(),
			);

			if *is_string {
				v10_result.unwrap_err();
			} else {
				v10_result.unwrap();
			}
		}
	}
}

#[test]
fn not_int_or_string_int_in_events() {
	let _guard = init_subscriber();

	let original_content = json!({
		"users": { alice(): 100 },
	});
	let original_content_object = extract_variant!(original_content, JsonValue::Object).unwrap();

	let current_room_power_levels_event = Some(default_room_power_levels());

	// Tuples of (is_string, is_int) booleans.
	let combinations = &[(true, false), (true, true), (false, true)];

	for (is_string, is_int) in combinations {
		info!(?is_string, ?is_int, "checking field");

		let value = match (is_string, is_int) {
			| (true, false) => "foo".into(),
			| (true, true) => "50".into(),
			| (false, true) => 50.into(),
			| _ => unreachable!(),
		};
		let mut events_object = JsonMap::new();
		events_object.insert("bar".to_owned(), value);

		let mut content_object = original_content_object.clone();
		content_object.insert("events".to_owned(), events_object.into());

		let incoming_event = to_pdu_event(
			"IPOWER2",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&content_object).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		);

		// String that is not a number is not accepted.
		let v6_result = check_room_power_levels(
			&RoomPowerLevelsEvent::new(incoming_event.clone()),
			current_room_power_levels_event.as_ref(),
			&AuthorizationRules::V6,
			int!(100),
			iter::empty(),
		);

		if *is_int {
			v6_result.unwrap();
		} else {
			v6_result.unwrap_err();
		}

		// String is not accepted.
		let v10_result = check_room_power_levels(
			&RoomPowerLevelsEvent::new(incoming_event),
			current_room_power_levels_event.as_ref(),
			&AuthorizationRules::V10,
			int!(100),
			iter::empty(),
		);

		if *is_string {
			v10_result.unwrap_err();
		} else {
			v10_result.unwrap();
		}
	}
}

#[test]
fn not_int_or_string_int_in_notifications() {
	let _guard = init_subscriber();

	let original_content = json!({
		"users": { alice(): 100 },
	});
	let original_content_object = extract_variant!(original_content, JsonValue::Object).unwrap();

	let current_room_power_levels_event = Some(default_room_power_levels());

	// Tuples of (is_string, is_int) booleans.
	let combinations = &[(true, false), (true, true), (false, true)];

	for (is_string, is_int) in combinations {
		info!(?is_string, ?is_int, "checking field");

		let value = match (is_string, is_int) {
			| (true, false) => "foo".into(),
			| (true, true) => "50".into(),
			| (false, true) => 50.into(),
			| _ => unreachable!(),
		};
		let mut notifications_object = JsonMap::new();
		notifications_object.insert("room".to_owned(), value);

		let mut content_object = original_content_object.clone();
		content_object.insert("notifications".to_owned(), notifications_object.into());

		let incoming_event = to_pdu_event(
			"IPOWER2",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&content_object).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		);

		// String that is not a number is not accepted.
		let v6_result = check_room_power_levels(
			&RoomPowerLevelsEvent::new(incoming_event.clone()),
			current_room_power_levels_event.as_ref(),
			&AuthorizationRules::V6,
			int!(100),
			iter::empty(),
		);

		if *is_int {
			v6_result.unwrap();
		} else {
			v6_result.unwrap_err();
		}

		// String is not accepted.
		let v10_result = check_room_power_levels(
			&RoomPowerLevelsEvent::new(incoming_event),
			current_room_power_levels_event.as_ref(),
			&AuthorizationRules::V10,
			int!(100),
			iter::empty(),
		);

		if *is_string {
			v10_result.unwrap_err();
		} else {
			v10_result.unwrap();
		}
	}
}

#[test]
fn not_user_id_in_users() {
	let _guard = init_subscriber();

	let content = json!({
		"users": {
			alice(): 100,
			"spambot": -1,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	let current_room_power_levels_event = Some(default_room_power_levels());

	// Key that is not a user ID is not accepted.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		current_room_power_levels_event.as_ref(),
		&AuthorizationRules::V6,
		int!(100),
		iter::empty(),
	)
	.unwrap_err();
}

#[test]
fn not_int_or_string_int_in_users() {
	let _guard = init_subscriber();

	let original_content = json!({
		"users": {
			alice(): 100,
		},
	});
	let original_content_object = extract_variant!(original_content, JsonValue::Object).unwrap();

	let current_room_power_levels_event = Some(default_room_power_levels());

	// Tuples of (is_string, is_int) booleans.
	let combinations = &[(true, false), (true, true), (false, true)];

	for (is_string, is_int) in combinations {
		info!(?is_string, ?is_int, "checking field");

		let value = match (is_string, is_int) {
			| (true, false) => "foo".into(),
			| (true, true) => "50".into(),
			| (false, true) => 50.into(),
			| _ => unreachable!(),
		};

		let mut content_object = original_content_object.clone();
		let users_object = content_object
			.get_mut("users")
			.unwrap()
			.as_object_mut()
			.unwrap();

		users_object.insert("@bar:baz".to_owned(), value);

		let incoming_event = to_pdu_event(
			"IPOWER2",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&content_object).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		);

		// String that is not a number is not accepted.
		let v6_result = check_room_power_levels(
			&RoomPowerLevelsEvent::new(incoming_event.clone()),
			current_room_power_levels_event.as_ref(),
			&AuthorizationRules::V6,
			int!(100),
			iter::empty(),
		);

		if *is_int {
			v6_result.unwrap();
		} else {
			v6_result.unwrap_err();
		}

		// String is not accepted.
		let v10_result = check_room_power_levels(
			&RoomPowerLevelsEvent::new(incoming_event),
			current_room_power_levels_event.as_ref(),
			&AuthorizationRules::V10,
			int!(100),
			iter::empty(),
		);

		if *is_string {
			v10_result.unwrap_err();
		} else {
			v10_result.unwrap();
		}
	}
}

#[test]
fn first_power_levels_event() {
	let _guard = init_subscriber();

	let content = json!({
		"users": {
			alice(): 100,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let current_room_power_levels_event = None::<RoomPowerLevelsEvent<PduEvent>>;

	// First power levels event is accepted.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		current_room_power_levels_event.as_ref(),
		&AuthorizationRules::V6,
		int!(100),
		iter::empty(),
	)
	.unwrap();
}

#[test]
fn change_content_level_with_current_higher_power_level() {
	let _guard = init_subscriber();

	let original_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
	});
	let original_content_object = extract_variant!(original_content, JsonValue::Object).unwrap();

	let int_fields = &[
		"users_default",
		"events_default",
		"state_default",
		"ban",
		"redact",
		"kick",
		"invite",
	];

	for field in int_fields {
		info!(?field, "checking field");

		let current_value = 60;
		let incoming_value = 40;

		let mut current_content_object = original_content_object.clone();
		current_content_object.insert((*field).to_owned(), current_value.into());

		let current_room_power_levels_event = to_pdu_event(
			"IPOWER",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&current_content_object).unwrap(),
			&["CREATE", "IMA"],
			&["IMA"],
		);

		let mut incoming_content_object = original_content_object.clone();
		incoming_content_object.insert((*field).to_owned(), incoming_value.into());
		let incoming_event = to_pdu_event(
			"IPOWER2",
			bob(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&incoming_content_object).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		);

		// Cannot change from a power level that is higher than the user.
		check_room_power_levels(
			&RoomPowerLevelsEvent::new(incoming_event),
			Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
			&AuthorizationRules::V6,
			int!(40),
			iter::empty(),
		)
		.unwrap_err();
	}
}

#[test]
fn change_content_level_with_new_higher_power_level() {
	let _guard = init_subscriber();

	let original_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
	});
	let original_content_object = extract_variant!(original_content, JsonValue::Object).unwrap();

	let int_fields = &[
		"users_default",
		"events_default",
		"state_default",
		"ban",
		"redact",
		"kick",
		"invite",
	];

	for field in int_fields {
		info!(?field, "checking field");

		let current_value = 40;
		let incoming_value = 60;

		let mut current_content_object = original_content_object.clone();
		current_content_object.insert((*field).to_owned(), current_value.into());

		let current_room_power_levels_event = to_pdu_event(
			"IPOWER",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&current_content_object).unwrap(),
			&["CREATE", "IMA"],
			&["IMA"],
		);

		let mut incoming_content_object = original_content_object.clone();
		incoming_content_object.insert((*field).to_owned(), incoming_value.into());
		let incoming_event = to_pdu_event(
			"IPOWER2",
			bob(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&incoming_content_object).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		);

		// Cannot change to a power level that is higher than the user.
		check_room_power_levels(
			&RoomPowerLevelsEvent::new(incoming_event),
			Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
			&AuthorizationRules::V6,
			int!(40),
			iter::empty(),
		)
		.unwrap_err();
	}
}

#[test]
fn change_content_level_with_same_power_level() {
	let _guard = init_subscriber();

	let original_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
	});
	let original_content_object = extract_variant!(original_content, JsonValue::Object).unwrap();

	let int_fields = &[
		"users_default",
		"events_default",
		"state_default",
		"ban",
		"redact",
		"kick",
		"invite",
	];

	for field in int_fields {
		info!(?field, "checking field");

		let current_value = 30;
		let incoming_value = 40;

		let mut current_content_object = original_content_object.clone();
		current_content_object.insert((*field).to_owned(), current_value.into());

		let current_room_power_levels_event = to_pdu_event(
			"IPOWER",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&current_content_object).unwrap(),
			&["CREATE", "IMA"],
			&["IMA"],
		);

		let mut incoming_content_object = original_content_object.clone();
		incoming_content_object.insert((*field).to_owned(), incoming_value.into());
		let incoming_event = to_pdu_event(
			"IPOWER2",
			bob(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&incoming_content_object).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		);

		// Can change a power level that is the same or lower than the user.
		check_room_power_levels(
			&RoomPowerLevelsEvent::new(incoming_event),
			Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
			&AuthorizationRules::V6,
			int!(40),
			iter::empty(),
		)
		.unwrap();
	}
}

#[test]
fn change_events_level_with_current_higher_power_level() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"events": {
			"foo": 60,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"events": {
			"foo": 40,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		bob(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Cannot change from a power level that is higher than the user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V6,
		int!(40),
		iter::empty(),
	)
	.unwrap_err();
}

#[test]
fn change_events_level_with_new_higher_power_level() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"events": {
			"foo": 30,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"events": {
			"foo": 60,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		bob(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Cannot change to a power level that is higher than the user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V6,
		int!(40),
		iter::empty(),
	)
	.unwrap_err();
}

#[test]
fn change_events_level_with_same_power_level() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"events": {
			"foo": 40,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"events": {
			"foo": 10,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		bob(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Can change a power level that is the same or lower than the user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V6,
		int!(40),
		iter::empty(),
	)
	.unwrap();
}

#[test]
fn change_notifications_level_with_current_higher_power_level() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"notifications": {
			"room": 60,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"notifications": {
			"room": 40,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		bob(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Notifications are not checked before v6.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event.clone()),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event.clone())),
		&AuthorizationRules::V3,
		int!(40),
		iter::empty(),
	)
	.unwrap();

	// Cannot change from a power level that is higher than the user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V6,
		int!(40),
		iter::empty(),
	)
	.unwrap_err();
}

#[test]
fn change_notifications_level_with_new_higher_power_level() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"notifications": {
			"room": 30,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"notifications": {
			"room": 60,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		bob(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Notifications are not checked before v6.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event.clone()),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event.clone())),
		&AuthorizationRules::V3,
		int!(40),
		iter::empty(),
	)
	.unwrap();

	// Cannot change to a power level that is higher than the user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V6,
		int!(40),
		iter::empty(),
	)
	.unwrap_err();
}

#[test]
fn change_notifications_level_with_same_power_level() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"notifications": {
			"room": 30,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
		"notifications": {
			"room": 31,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		bob(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Notifications are not checked before v6.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event.clone()),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event.clone())),
		&AuthorizationRules::V3,
		int!(40),
		iter::empty(),
	)
	.unwrap();

	// Can change a power level that is the same or lower than the user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V6,
		int!(40),
		iter::empty(),
	)
	.unwrap();
}

#[test]
fn change_other_user_level_with_current_higher_power_level() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
			zara(): 70,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		bob(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Cannot change from a power level that is higher than the user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V6,
		int!(40),
		iter::empty(),
	)
	.unwrap_err();
}

#[test]
fn change_other_user_level_with_new_higher_power_level() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
			zara(): 10,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
			zara(): 45,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		bob(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Cannot change to a power level that is higher than the user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V6,
		int!(40),
		iter::empty(),
	)
	.unwrap_err();
}

#[test]
fn change_other_user_level_with_same_power_level() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
			zara(): 20,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
			zara(): 40,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		bob(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Can change a power level that is the same or lower than the user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V6,
		int!(40),
		iter::empty(),
	)
	.unwrap();
}

#[test]
fn change_own_user_level_to_new_higher_power_level() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 100,
			bob(): 100,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		bob(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Cannot change to a power level that is higher than the user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V6,
		int!(40),
		iter::empty(),
	)
	.unwrap_err();
}

#[test]
fn change_own_user_level_to_lower_power_level() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			alice(): 100,
			bob(): 40,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 100,
			bob(): 20,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		bob(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Can change own power level to a lower level than the user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V6,
		int!(40),
		iter::empty(),
	)
	.unwrap();
}

#[test]
fn creator_has_infinite_power() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			bob(): i64::from(ruma::Int::MAX),
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			bob(): 0,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Room creator has infinite power level, and hence can change the power level
	// of any other user.
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V12,
		UserPowerLevel::Infinite,
		iter::empty(),
	)
	.unwrap();
}

#[test]
fn dont_allow_creator_in_users_field() {
	let _guard = init_subscriber();

	let current_content = json!({
		"users": {
			bob(): 40,
		},
	});
	let current_room_power_levels_event = to_pdu_event(
		"IPOWER",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&current_content).unwrap(),
		&["CREATE", "IMA"],
		&["IMA"],
	);

	let incoming_content = json!({
		"users": {
			alice(): 10,
			bob(): 40,
		},
	});
	let incoming_event = to_pdu_event(
		"IPOWER2",
		alice(),
		TimelineEventType::RoomPowerLevels,
		Some(""),
		to_raw_json_value(&incoming_content).unwrap(),
		&["CREATE", "IMA", "IPOWER"],
		&["IPOWER"],
	);

	// Room creator cannot be in the `users` field of the power levels event
	check_room_power_levels(
		&RoomPowerLevelsEvent::new(incoming_event),
		Some(&RoomPowerLevelsEvent::new(current_room_power_levels_event)),
		&AuthorizationRules::V12,
		UserPowerLevel::Infinite,
		iter::once(alice().to_owned()),
	)
	.unwrap_err();
}
