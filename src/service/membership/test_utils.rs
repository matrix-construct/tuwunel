use ruma::{RoomId, UserId};
use serde_json::{Value as JsonValue, json};

/// Builds a resident's v11 membership template with an `unsigned` map, then
/// with a non-map `unsigned`.
///
/// The map holds keys a client reads on the event as this server's own.
pub(super) fn templates_with_unsigned(
	room_id: &RoomId,
	user_id: &UserId,
	membership: &str,
) -> impl Iterator<Item = JsonValue> {
	let planted = json!({"transaction_id": "planted", "prev_content": {"membership": "invite"}});

	[planted, json!("planted")]
		.into_iter()
		.map(move |unsigned| {
			json!({
				"type": "m.room.member", "room_id": room_id, "sender": user_id, "state_key": user_id,
				"content": {"membership": membership}, "origin_server_ts": 1, "depth": 3,
				"prev_events": ["$prev"], "auth_events": ["$auth"], "unsigned": unsigned,
			})
		})
}
