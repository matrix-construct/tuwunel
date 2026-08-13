use super::{Count, RawId, ShortRoomId};

/// Typed components of a PDU database key.
///
/// The room surrogate scopes the key and the count locates the event within the
/// normal or backfilled timeline. Conversion to `RawId` packs both components.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Id {
	/// Compact local identifier of the event's room.
	///
	/// This is encoded first so room-scoped database scans share a fixed
	/// prefix.
	pub shortroomid: ShortRoomId,

	/// Timeline sequence assigned to the event.
	///
	/// The count selects the normal or backfilled raw-key layout when encoded.
	pub count: Count,
}

impl From<RawId> for Id {
	#[inline]
	fn from(raw: RawId) -> Self {
		Self {
			shortroomid: u64::from_be_bytes(raw.shortroomid()),
			count: Count::from_unsigned(u64::from_be_bytes(raw.count())),
		}
	}
}
