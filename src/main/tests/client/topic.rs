//! Room topic writes for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it. A binary declaring it also declares the client harness and its
//! state module, whose `Client` and `put_state` it builds on.

use serde_json::json;
use tuwunel_core::{
	Result, implement,
	ruma::{OwnedEventId, RoomId},
};

use super::client::Client;

/// Set one room's topic as this user and return the event's ID.
///
/// A non-success status is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn set_topic(&self, room: &RoomId, topic: &str) -> Result<OwnedEventId> {
	self.put_state(room, "m.room.topic", "", &json!({ "topic": topic }))
		.await
}
