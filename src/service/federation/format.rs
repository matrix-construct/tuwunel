//! Outgoing federation event formatting.
//!
//! The formatter converts a canonical event object into the room-version shape
//! expected by ruma's outgoing federation response types.

use futures::future::OptionFuture;
use ruma::{CanonicalJsonObject, CanonicalJsonValue, RoomId, RoomVersionId};
use serde_json::value::{RawValue as RawJsonValue, to_raw_value};
use tuwunel_core::{implement, matrix::pdu, utils::result::FlatOk};

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
