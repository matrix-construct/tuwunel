use ruma::{RoomVersionId, events::room::create::RoomCreateEventContent};
pub use ruma::{RoomVersionId as RoomVersion, room_version_rules::RoomVersionRules};

use crate::{Result, err, matrix::Event};

/// Resolves the rule set for a supported room version.
///
/// Known versions return Ruma's complete authorization, redaction, and
/// event-format rules. Custom or unsupported identifiers produce an
/// unsupported-version result.
pub fn rules(room_version_id: &RoomVersionId) -> Result<RoomVersionRules> {
	room_version_id.rules().ok_or_else(|| {
		err!(Request(UnsupportedRoomVersion(
			"Unknown or unsupported room version {room_version_id:?}.",
		)))
	})
}

/// Extracts the room version declared by a room creation event.
///
/// The event content is deserialized as `m.room.create` content. The returned
/// identifier is owned independently of the event.
pub fn from_create_event<Pdu: Event>(create_event: &Pdu) -> Result<RoomVersionId> {
	let content: RoomCreateEventContent = create_event.get_content()?;
	Ok(from_create_content(&content).clone())
}

/// Borrows the room version from parsed room creation content.
///
/// Ruma applies the protocol default while deserializing content that omits the
/// field. This accessor performs no additional validation.
#[inline]
#[must_use]
pub fn from_create_content(content: &RoomCreateEventContent) -> &RoomVersionId {
	&content.room_version
}
