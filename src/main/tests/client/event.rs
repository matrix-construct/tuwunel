//! Event sending on the shared test `Client`.
//!
//! Kept apart from the client harness so that binaries which never send events
//! do not compile it. A binary declaring it also declares the client harness
//! and its `put` module, whose `Client::put` it builds on.

use serde_json::{Value, json};
use tuwunel_core::{
	Result, implement,
	ruma::{EventId, OwnedEventId, RoomId},
};

use super::client::{Client, field};

/// Send a text message to a room as this user and return its ID.
///
/// The transaction ID must be new for this user's client; a reused one
/// returns the earlier event's ID and the new body is dropped.
#[implement(Client, params = "<'_>")]
pub(crate) async fn send_text(
	&self,
	room_id: &RoomId,
	txn_id: &str,
	body: &str,
) -> Result<OwnedEventId> {
	let message = json!({ "msgtype": "m.text", "body": body });
	let path = format!("rooms/{room_id}/send/m.room.message/{txn_id}");

	self.put_event(&path, &message).await
}

/// Put an event through the client API as this user and return its ID.
///
/// The path names the event's room and type, and a refusal is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn put_event(&self, path: &str, content: &Value) -> Result<OwnedEventId> {
	let response = self.put(path, content).await?;

	Ok(EventId::parse(field(&response, "event_id")?)?)
}
