//! Timeline writes shared by the server-booting tests that opt in.
//!
//! Each goes straight through the timeline service under the room's state
//! lock, so a test can put any event in a room without a client round trip.

use tuwunel_core::{
	Result,
	ruma::{OwnedEventId, RoomId, UserId, events::room::message::RoomMessageEventContent},
};
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::Services;

/// Append a message to a room as `sender` and return its event id.
///
/// A message an admin sends into the admin room is queued as a command just as
/// one arriving from a client would be.
pub(crate) async fn append_message(
	services: &Services,
	sender: &UserId,
	room_id: &RoomId,
	content: &RoomMessageEventContent,
) -> Result<OwnedEventId> {
	append_pdu(services, sender, room_id, PduBuilder::timeline(content)).await
}

/// Append the event `builder` describes to a room as `sender` and return its
/// event id.
///
/// The event passes the room's auth rules like any other, so a state event
/// such as a membership lands only where its sender may send it.
pub(crate) async fn append_pdu(
	services: &Services,
	sender: &UserId,
	room_id: &RoomId,
	builder: PduBuilder,
) -> Result<OwnedEventId> {
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(builder, sender, room_id, &state_lock)
		.await
}
