//! Membership actions on the shared test `Client`.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code. A
//! binary declaring it also declares the client harness, whose `Client` it
//! extends.

use serde_json::json;
use tuwunel_core::{
	Result, implement,
	ruma::{RoomId, UserId},
};

use super::client::Client;

/// Apply a membership action to a user in a room, as this user.
///
/// The action names the endpoint, such as `invite`, `kick` or `ban`, and a
/// refusal is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn act_on(&self, room_id: &RoomId, action: &str, user_id: &UserId) -> Result {
	let path = format!("rooms/{room_id}/{action}");

	self.post(&path, &json!({ "user_id": user_id }))
		.await?;

	Ok(())
}
