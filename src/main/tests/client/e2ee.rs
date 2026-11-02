//! Encrypted-room scaffolding for the sync tests that declare this module by
//! path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code. A binary
//! declaring it also declares the client harness and the sliding sync module,
//! whose `Client` and `sliding_sync` it builds on.

use serde_json::{Value, json};
use tuwunel_core::{Result, implement, ruma::OwnedRoomId};

use super::client::Client;

/// How long a resumed sliding sync polls for, in milliseconds.
///
/// Every resumed round follows a change already written, so the poll answers
/// on its first pass and the budget only bounds a round that found nothing.
const POLL_TIMEOUT: u64 = 1_500;

/// Create an invite-only room with encryption in its initial state.
///
/// Encryption puts a member's join into the device lists, and the
/// `private_chat` preset makes every join follow an invite.
#[implement(Client, params = "<'_>")]
pub(crate) async fn create_encrypted_room(&self) -> Result<OwnedRoomId> {
	let encryption = json!({
		"type": "m.room.encryption",
		"state_key": "",
		"content": { "algorithm": "m.megolm.v1.aes-sha2" },
	});

	let body = json!({ "preset": "private_chat", "initial_state": [encryption] });

	self.create_room(&body).await
}

/// Run one sliding sync with the e2ee extension, resuming from `pos` if given.
///
/// The single list keeps the user's rooms in the connection's window and asks
/// for the given required state, which can prove a round visited a room.
#[implement(Client, params = "<'_>")]
pub(crate) async fn sync_e2ee(&self, pos: Option<&str>, required_state: &Value) -> Result<Value> {
	let body = json!({
		"lists": {
			"all": {
				"ranges": [[0, 9]],
				"required_state": required_state,
				"timeline_limit": 0,
			},
		},
		"extensions": {
			"e2ee": { "enabled": true },
		},
	});

	self.sliding_sync(pos, POLL_TIMEOUT, &body).await
}
