//! The `createRoom` body of a restricted room, for the tests that join one.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it.

use serde_json::{Value, json};
use tuwunel_core::ruma::RoomId;

/// A restricted room's `createRoom` body admitting `allowed`'s members.
///
/// The initial join rule overrides the `private_chat` preset's, so only
/// membership of the allowed room lets a user in without an invite.
pub(crate) fn restricted(allowed: &RoomId) -> Value {
	json!({
		"preset": "private_chat",
		"initial_state": [{
			"type": "m.room.join_rules",
			"state_key": "",
			"content": {
				"join_rule": "restricted",
				"allow": [{ "type": "m.room_membership", "room_id": allowed }],
			},
		}],
	})
}
