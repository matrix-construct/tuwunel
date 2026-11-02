//! Redactions for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it. A binary declaring it also declares the client harness and its
//! PUT and event modules, whose `Client` and `put_event` it builds on.

use serde_json::json;
use tuwunel_core::{
	Result, implement,
	ruma::{EventId, OwnedEventId, RoomId},
};

use super::client::Client;

/// Redact an event as this user and return the redaction's ID.
///
/// The transaction ID names the redaction; a reused one for the same target
/// returns the earlier redaction's ID. A refusal is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn redact(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	txn_id: &str,
) -> Result<OwnedEventId> {
	let path = format!("rooms/{room_id}/redact/{event_id}/{txn_id}");

	self.put_event(&path, &json!({})).await
}
