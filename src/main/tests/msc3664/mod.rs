//! Endpoints and assertions particular to the MSC3664 push condition tests.
//!
//! The generic half of the harness lives in `crate::client`; both MSC3664
//! binaries use every item here, which keeps the module free of dead code in
//! either.

use std::time::Duration;

use serde_json::{Value, json};
use tuwunel_core::{
	Result, implement,
	ruma::{OwnedEventId, RoomId, UserId},
};
use tuwunel_service::Services;

use crate::client::{Client, poll_until};

/// The unstable kind of the push condition under test.
pub(crate) const CONDITION_KIND: &str = "im.nheko.msc3664.related_event_match";

const PUSH_DEADLINE: Duration = Duration::from_secs(5);

#[implement(Client, params = "<'_>")]
pub(crate) async fn set_push_rule(&self, rule_id: &str, rule: &Value) -> Result {
	self.put(&format!("pushrules/global/override/{rule_id}"), rule)
		.await?;

	Ok(())
}

#[implement(Client, params = "<'_>")]
pub(crate) async fn send(
	&self,
	room_id: &RoomId,
	event_type: &str,
	txn_id: &str,
	content: &Value,
) -> Result<OwnedEventId> {
	let path = format!("rooms/{room_id}/send/{event_type}/msc3664-{txn_id}");

	self.put_event(&path, content).await
}

/// The server's advertised capability for the condition, absent when the
/// server does not evaluate it.
#[implement(Client, params = "<'_>")]
pub(crate) async fn condition_capability(&self) -> Result<Option<Value>> {
	let response = self.get("capabilities").await?;
	let capability = response
		.get("capabilities")
		.and_then(|capabilities| capabilities.get(CONDITION_KIND))
		.cloned();

	Ok(capability)
}

/// The body every room in these tests is created from.
///
/// Both binaries drive the identical room shape, so the enabled and disabled
/// halves of the gate stay comparable.
pub(crate) fn public_room() -> Value { json!({ "preset": "public_chat" }) }

/// Whether the room's notification count reaches `want` before the deadline.
///
/// Push evaluation trails the send response, so the count is polled rather
/// than sampled once.
pub(crate) async fn notified(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	want: u64,
) -> bool {
	poll_until(PUSH_DEADLINE, async || {
		services
			.pusher
			.notification_count(user_id, room_id)
			.await
			.ge(&want)
	})
	.await
}

/// Whether the room's highlight count reaches `want` before the deadline.
///
/// A rule asking for the highlight tweak is the way to tell its own match from
/// the default rule that notifies for every message, which would otherwise
/// satisfy an assertion on the notification count alone.
pub(crate) async fn highlighted(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	want: u64,
) -> bool {
	poll_until(PUSH_DEADLINE, async || {
		services
			.pusher
			.highlight_count(user_id, room_id)
			.await
			.ge(&want)
	})
	.await
}
