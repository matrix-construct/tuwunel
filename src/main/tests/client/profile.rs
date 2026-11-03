//! Profile writes on the shared test `Client`.
//!
//! Kept apart from the client harness so that only a binary declaring it by
//! path compiles it. A binary declaring it also declares the client harness and
//! its `put` module, whose `Client::put` it builds on.

use serde_json::json;
use tuwunel_core::{Result, implement, ruma::UserId};

use super::client::Client;

/// Set a user's display name as this user.
///
/// The server sends the user's new member event into each room they have
/// joined, and the call fails if the server refuses the request.
#[implement(Client, params = "<'_>")]
pub(crate) async fn rename(&self, user_id: &UserId, displayname: &str) -> Result {
	let body = json!({ "displayname": displayname });

	self.put(&format!("profile/{user_id}/displayname"), &body)
		.await?;

	Ok(())
}
