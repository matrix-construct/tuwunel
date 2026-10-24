//! Membership actions on the shared test `Client`, and the room path they use.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code, which is
//! also why the room path lives here rather than beside the harness's `url`. A
//! binary declaring it also declares the client harness, whose `Client` it
//! extends.

use serde_json::json;
use tuwunel_core::{
	Result, implement,
	ruma::{RoomId, UserId},
};

use super::client::Client;

/// Invite a user to a room as this user, then join it as them.
///
/// The invitee's own client accepts, so the user ends up joined through the
/// same requests a real client sends; either refusal is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn admit(
	&self,
	room_id: &RoomId,
	invitee: &Client<'_>,
	invitee_id: &UserId,
) -> Result {
	self.act_on(room_id, "invite", invitee_id).await?;

	invitee.act(room_id, "join").await
}

/// Apply a membership action to a user in a room, as this user.
///
/// The action names the endpoint, such as `invite`, `kick` or `ban`, and a
/// refusal is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn act_on(&self, room_id: &RoomId, action: &str, user_id: &UserId) -> Result {
	self.post(&room_path(room_id, action), &json!({ "user_id": user_id }))
		.await
		.map(drop)
}

/// Apply a membership action to this user, such as a join or a leave.
///
/// The membership is written before the response returns, so every request
/// that follows sees it; a refusal is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn act(&self, room_id: &RoomId, action: &str) -> Result {
	self.post(&room_path(room_id, action), &json!({}))
		.await
		.map(drop)
}

/// The client-API path of one endpoint under a room.
///
/// The endpoint is appended as given, so it may carry further segments, such
/// as a state event type and key.
pub(crate) fn room_path(room_id: &RoomId, endpoint: &str) -> String {
	format!("rooms/{room_id}/{endpoint}")
}
