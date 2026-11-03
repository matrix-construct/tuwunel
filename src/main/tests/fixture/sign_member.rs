//! Member-event signing for the tests that hand a user's own membership event
//! to the federation ingest.
//!
//! A binary declaring it by path declares `sign` beside it, which this module
//! imports.

use tuwunel_core::{
	Result,
	ruma::{
		CanonicalJsonObject, OwnedEventId, RoomId, UserId,
		events::room::member::RoomMemberEventContent,
	},
};
use tuwunel_matrix::pdu::{PduBuilder, into_outgoing_federation};
use tuwunel_service::Services;

use super::sign::sign;

/// A signed event keyed by its ID, in the form a federation peer delivers it.
///
/// The object is the event as `into_outgoing_federation` leaves it.
pub(crate) type SignedEvent = (OwnedEventId, CanonicalJsonObject);

/// Build and sign a user's own member event without appending it.
///
/// It comes back in federation form, so a test can ingest a join or a profile
/// change as a peer would deliver it.
pub(crate) async fn sign_member(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	content: &RoomMemberEventContent,
) -> Result<SignedEvent> {
	let room_version = services.state.get_room_version(room_id).await?;
	let builder = PduBuilder::state(user_id.as_str(), content);
	let (pdu, signed) = sign(services, user_id, room_id, builder).await?;

	Ok((pdu.event_id, into_outgoing_federation(signed, &room_version)))
}
