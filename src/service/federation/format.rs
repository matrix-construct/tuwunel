//! Outgoing federation event formatting.
//!
//! The formatter converts a canonical event object, or a stored event read by
//! its ID, into the room-version shape expected by ruma's outgoing federation
//! response types.

use futures::future::OptionFuture;
use ruma::{CanonicalJsonObject, CanonicalJsonValue, EventId, RoomId, RoomVersionId};
use serde_json::value::{RawValue as RawJsonValue, to_raw_value};
use tuwunel_core::{Result, implement, utils::result::FlatOk};
use tuwunel_matrix::pdu;

/// Reads a stored event and formats it for an outgoing federation response.
///
/// The read's error is returned unchanged, so each caller keeps its own policy
/// for an event it cannot serve; the formatting itself cannot fail.
#[implement(super::Service)]
pub async fn get_pdu_into(
	&self,
	event_id: &EventId,
	room_version: Option<&RoomVersionId>,
) -> Result<Box<RawJsonValue>> {
	let pdu_json = self
		.services
		.timeline
		.get_pdu_json(event_id)
		.await?;

	Ok(self.format_pdu_into(pdu_json, room_version).await)
}

/// Formats an event object for an outgoing federation response.
///
/// The supplied room version takes precedence; otherwise the room ID is used to
/// look it up. When no version can be resolved, only `event_id` is removed.
/// The returned raw JSON satisfies ruma's response type and is not a full PDU.
#[implement(super::Service)]
pub async fn format_pdu_into(
	&self,
	mut pdu_json: CanonicalJsonObject,
	room_version: Option<&RoomVersionId>,
) -> Box<RawJsonValue> {
	let room_id = pdu_json
		.get("room_id")
		.and_then(CanonicalJsonValue::as_str)
		.map(RoomId::parse)
		.flat_ok();

	let query_room_version: OptionFuture<_> = room_id
		.filter(|_| room_version.is_none())
		.map(async |room_id| {
			self.services
				.state
				.get_room_version(&room_id)
				.await
				.ok()
		})
		.into();

	if let Some(room_version) = query_room_version
		.await
		.flatten()
		.as_ref()
		.or(room_version)
	{
		pdu_json = pdu::into_outgoing_federation(pdu_json, room_version);
	} else {
		pdu_json.remove("event_id");
	}

	to_raw_value(&pdu_json).expect("CanonicalJson is valid serde_json::Value")
}
