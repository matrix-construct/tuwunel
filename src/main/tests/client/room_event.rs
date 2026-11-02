//! Fetching one room event, for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it. A binary declaring it also declares the client harness and its
//! GET module, whose `Client` and `get` it builds on.

use serde_json::Value;
use tuwunel_core::{
	Result, implement,
	ruma::{EventId, RoomId},
};

use super::client::Client;

/// Fetch one event of a room as this user and parse the reply.
///
/// The event comes back as the client API serves it to this user, `unsigned`
/// included. A refusal is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn event(&self, room_id: &RoomId, event_id: &EventId) -> Result<Value> {
	self.get(&format!("rooms/{room_id}/event/{event_id}"))
		.await
}
