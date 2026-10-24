//! Event context requests, and readers for the state their replies carry.
//!
//! It lives outside the client harness, so only a binary that declares it
//! compiles it; that binary also declares the harness and its GET and admin API
//! modules, which supply `Client`, `get_url` and `admin_api`.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tuwunel_core::{
	Result, implement,
	ruma::{EventId, RoomId, UserId},
};

use super::client::Client;

/// Get the context of one event as this user, with up to `limit` events
/// around it.
///
/// The status comes back beside the reply's body, so a refusal is a status to
/// assert rather than an error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn context(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	limit: usize,
) -> Result<(StatusCode, Value)> {
	let path = format!("rooms/{room_id}/context/{event_id}?limit={limit}");
	let response = self.get_url(&self.url(&path)).await?;
	let status = response.status();
	let body = response.json().await?;

	Ok((status, body))
}

/// Get the context of one event through the admin API, with up to `limit`
/// events around it.
///
/// The admin API bypasses the requester's visibility, so a refusal is a
/// failure rather than a case to assert.
#[implement(Client, params = "<'_>")]
pub(crate) async fn admin_context(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	limit: usize,
) -> Result<Value> {
	let path = format!("v1/rooms/{room_id}/context/{event_id}?limit={limit}");
	let reply = self
		.admin_api(Method::GET, &path, &json!({}), 200)
		.await?;

	Ok(reply.parse()?)
}

/// The topic among the state events a context reply carries.
///
/// There is none when the reply carries no topic, as when the requester may
/// read no state.
pub(crate) fn topic_of(reply: &Value) -> Option<&str> {
	state_event(reply, "m.room.topic", "")?["content"]["topic"].as_str()
}

/// A user's membership among the state events a context reply carries.
///
/// There is none when the reply carries no member event for the user.
pub(crate) fn membership_of<'a>(reply: &'a Value, user_id: &UserId) -> Option<&'a str> {
	state_event(reply, "m.room.member", user_id.as_str())?["content"]["membership"].as_str()
}

fn state_event<'a>(reply: &'a Value, kind: &str, state_key: &str) -> Option<&'a Value> {
	reply["state"]
		.as_array()?
		.iter()
		.find(|event| event["type"] == kind && event["state_key"] == state_key)
}
