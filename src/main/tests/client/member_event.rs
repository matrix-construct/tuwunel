//! Member-event matching for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code.

use serde_json::Value;
use tuwunel_core::ruma::UserId;

/// The first event of a response body's list that is the member event about
/// one user.
///
/// A value that is not a list holds no such event.
pub(crate) fn find_member_event<'a>(events: &'a Value, user_id: &UserId) -> Option<&'a Value> {
	events
		.as_array()
		.into_iter()
		.flatten()
		.find(|event| is_member_event(event, user_id))
}

/// Whether an event in a response body is the member event about one user.
///
/// The event's content is not read, so any membership matches.
pub(crate) fn is_member_event(event: &Value, user_id: &UserId) -> bool {
	event["type"] == "m.room.member" && event["state_key"] == user_id.as_str()
}
