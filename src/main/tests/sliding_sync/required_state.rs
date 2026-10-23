//! Required state assertions shared by the tests that check its delivery.
//!
//! A test binary declares this file by path, beside the sliding sync module,
//! only when it asserts on required state, so that module's other users
//! compile none of it.

use serde_json::Value;
use tuwunel_core::{Result, err, ruma::RoomId, utils::BoolExt};

/// Fails unless a reply carries the room with exactly the expected required
/// state.
///
/// The events are named by ID in any order. An absent room fails, so an empty
/// expectation asserts the room came back without required state.
pub(crate) fn expect_state(
	response: &Value,
	room: &RoomId,
	expected: &[&str],
	context: &str,
) -> Result {
	let payload = &response["rooms"][room.as_str()];
	let state = payload["required_state"]
		.as_array()
		.map(Vec::as_slice)
		.unwrap_or_default();

	let matches = payload.is_object()
		&& state.len() == expected.len()
		&& expected
			.iter()
			.all(|id| state.iter().any(|event| event["event_id"] == *id));

	BoolExt::ok_or_else(matches, || err!("{context}: expected {expected:?}, received {payload}"))
}
