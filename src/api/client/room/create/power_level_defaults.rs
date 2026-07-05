use serde_json::{Map as JsonMap, Value as JsonValue};

/// Merge a top-level `m.room.power_levels` override object into the
/// power-levels content being built for a new room. Keys present in the
/// override replace the corresponding top-level keys (e.g. `users_default`,
/// `events`, `invite`), matching how the client `power_level_content_override`
/// is applied.
pub(super) fn merge_power_level_content_override(
	power_levels_content: &mut JsonValue,
	override_json: &JsonMap<String, JsonValue>,
) {
	let target = power_levels_content
		.as_object_mut()
		.expect("power levels content must serialize to an object");

	for (key, value) in override_json {
		target.insert(key.clone(), value.clone());
	}
}

#[cfg(test)]
mod tests {
	use super::super::*;

	#[test]
	fn default_power_levels_content_applies_server_default_override() {
		let rules = room_version::rules(&RoomVersionId::V11).expect("supported room version");

		let content = default_power_levels_content(
			&rules,
			Some(&json!({ "users_default": 50 })),
			None,
			&RoomPreset::PrivateChat,
			BTreeMap::new(),
		)
		.expect("power levels content");

		assert_eq!(content["users_default"], json!(50));
	}

	#[test]
	fn request_override_wins_over_server_default_override() {
		let rules = room_version::rules(&RoomVersionId::V11).expect("supported room version");
		let request_override =
			Raw::from_json(to_raw_value(&json!({ "users_default": 75 })).expect("raw json"));

		let content = default_power_levels_content(
			&rules,
			Some(&json!({ "users_default": 50 })),
			Some(&request_override),
			&RoomPreset::PrivateChat,
			BTreeMap::new(),
		)
		.expect("power levels content");

		assert_eq!(content["users_default"], json!(75));
	}

	#[test]
	fn default_override_preserves_explicit_user_power_levels() {
		let rules = room_version::rules(&RoomVersionId::V11).expect("supported room version");
		let creator = OwnedUserId::try_from("@alice:example.com").expect("valid user id");
		let users = BTreeMap::from([(creator.clone(), int!(100))]);

		let content = default_power_levels_content(
			&rules,
			Some(&json!({ "users_default": 50 })),
			None,
			&RoomPreset::PrivateChat,
			users,
		)
		.expect("power levels content");

		assert_eq!(content["users_default"], json!(50));
		assert_eq!(content["users"][creator.as_str()], json!(100));
	}
}
