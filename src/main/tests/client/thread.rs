//! Thread replies and the ignored-user list, for the tests that declare this
//! module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it. A binary declaring it also declares the client harness and its
//! PUT and event modules, whose `Client`, `put` and `put_event` it builds on.

use serde_json::json;
use tuwunel_core::{
	Result, implement,
	ruma::{EventId, OwnedEventId, RoomId, UserId},
};

use super::client::Client;

/// Reply in the thread rooted at `root` as this user and return the reply's ID.
///
/// The reply carries the fallback a client without thread support renders. The
/// transaction ID must be new for this user's client.
#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn reply(
	&self,
	room_id: &RoomId,
	txn_id: &str,
	root: &EventId,
) -> Result<OwnedEventId> {
	let content = json!({
		"msgtype": "m.text",
		"body": "thread reply",
		"m.relates_to": {
			"rel_type": "m.thread",
			"event_id": root,
			"is_falling_back": true,
			"m.in_reply_to": { "event_id": root },
		},
	});

	let path = format!("rooms/{room_id}/send/m.room.message/{txn_id}");

	self.put_event(&path, &content).await
}

/// Replace this user's ignored-user list with one naming only `ignored`.
///
/// The list is the user's own account data, so `user_id` must be this client's
/// user; the server applies it from the next request on.
#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn set_ignored(&self, user_id: &UserId, ignored: &UserId) -> Result {
	let path = format!("user/{user_id}/account_data/m.ignored_user_list");
	let list = json!({ "ignored_users": { ignored.as_str(): {} } });

	self.put(&path, &list).await?;

	Ok(())
}
