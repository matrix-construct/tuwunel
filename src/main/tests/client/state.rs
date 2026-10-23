//! Room state writes for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it. A binary declaring it also declares the client harness and its
//! PUT module, whose `Client` and `put` it builds on.

use serde_json::Value;
use tuwunel_core::{
	Result, implement,
	ruma::{OwnedEventId, RoomId},
};

use super::client::{Client, field};

/// Put one room state event as this user and return the event's ID.
///
/// An empty state key addresses the event type's only entry. A non-success
/// status is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn put_state(
	&self,
	room: &RoomId,
	event_type: &str,
	state_key: &str,
	content: &Value,
) -> Result<OwnedEventId> {
	let path = format!("rooms/{room}/state/{event_type}/{state_key}");
	let response = self.put(&path, content).await?;

	Ok(field(&response, "event_id")?.try_into()?)
}
