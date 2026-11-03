//! Room state snapshots on the shared test `Client`.
//!
//! Kept apart from the client harness so that only a binary declaring it by
//! path compiles it. A binary declaring it also declares the client harness and
//! its `event` module, whose `Client::send_text` it builds on.

use tuwunel_core::{Result, implement, ruma::RoomId};

use super::client::Client;

/// Send a text message as this user and return the room's state snapshot at it.
///
/// A message changes no state, so the snapshot holds the room's state when the
/// message was sent, which a test can later force the room back onto.
#[implement(Client, params = "<'_>")]
pub(crate) async fn state_after_text(
	&self,
	room_id: &RoomId,
	txn_id: &str,
	body: &str,
) -> Result<u64> {
	let event_id = self.send_text(room_id, txn_id, body).await?;

	self.services
		.state
		.pdu_shortstatehash(&event_id)
		.await
}
