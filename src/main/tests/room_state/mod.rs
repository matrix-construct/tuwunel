//! Current room state reads shared by the server-booting tests that opt in.
//!
//! Each goes straight to the running services, so a test can learn the ID of
//! a state event whose content is all the client API returns.

use tuwunel_core::{
	Result,
	ruma::{OwnedEventId, RoomId, events::StateEventType},
};
use tuwunel_service::Services;

/// The ID of one event in a room's current state.
///
/// An empty state key addresses the event type's only entry, and an absent
/// entry is the error.
pub(crate) async fn state_id(
	services: &Services,
	room_id: &RoomId,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<OwnedEventId> {
	services
		.state_accessor
		.room_state_get_id(room_id, event_type, state_key)
		.await
}
