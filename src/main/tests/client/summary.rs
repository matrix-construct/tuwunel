//! Room summary requests for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code. A
//! binary declaring it also declares the client harness, whose `Client` it
//! extends, and the GET include it sends through.

use reqwest::Response;
use tuwunel_core::{Result, implement, ruma::RoomId};

use super::client::Client;

/// Request a room's summary as this user and keep the raw reply.
///
/// The status stays the caller's to judge, so a refusal is as easy to assert
/// as a success.
#[implement(Client, params = "<'_>")]
pub(crate) async fn summary(&self, room_id: &RoomId) -> Result<Response> {
	let url = self.client_url("v1/", &format!("room_summary/{room_id}"));

	self.get_url(&url).await
}
