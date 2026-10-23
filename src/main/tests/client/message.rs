//! Room messages for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it. A binary declaring it also declares the client harness and its
//! PUT module, whose `Client` and `put` it builds on.

use serde_json::json;
use tuwunel_core::{
	Result, implement,
	ruma::{OwnedEventId, RoomId},
};

use super::client::{Client, field};

/// Send one text message as this user and return the event's ID.
///
/// The transaction ID names the send. A non-success status is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn send_text(
	&self,
	room: &RoomId,
	txn_id: &str,
	body: &str,
) -> Result<OwnedEventId> {
	let path = format!("rooms/{room}/send/m.room.message/{txn_id}");
	let response = self
		.put(&path, &json!({ "msgtype": "m.text", "body": body }))
		.await?;

	Ok(field(&response, "event_id")?.try_into()?)
}
