//! Event signing for the tests that hand a locally built event to the
//! federation ingest, as a peer would deliver it.
//!
//! Only a binary declaring it by path compiles it, so the boot fixture's other
//! binaries stay free of its dead code.

use tuwunel_core::{
	Result,
	ruma::{CanonicalJsonObject, RoomId, UserId},
};
use tuwunel_matrix::{PduEvent, pdu::PduBuilder};
use tuwunel_service::Services;

/// Build and sign an event without appending it.
///
/// The room's state lock is held only while the event is built, so the ingest
/// that later handles it can take the lock.
pub(crate) async fn sign(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	builder: PduBuilder,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await
}
