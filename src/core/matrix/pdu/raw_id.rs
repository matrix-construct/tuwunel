use std::fmt;

use arrayvec::ArrayVec;
use serde::{Deserialize, Deserializer};

use super::{
	super::{ShortId, ShortRoomId},
	Count, Id,
};

/// Packed database key for a room's normal or backfilled PDU.
///
/// Both layouts begin with the big-endian compact room identifier. Normal keys
/// are 16 bytes and backfilled keys are 24 bytes.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub enum RawId {
	/// Packed key for an event in the normal timeline.
	///
	/// The key concatenates the room surrogate and unsigned count as two
	/// big-endian integers.
	Normal(RawIdNormal),

	/// Packed key for an event in the backfilled timeline.
	///
	/// The key inserts an eight-byte zero marker between the room surrogate and
	/// signed count bits.
	Backfilled(RawIdBackfilled),
}

type RawIdNormal = [u8; RawId::NORMAL_LEN];
type RawIdBackfilled = [u8; RawId::BACKFILLED_LEN];

struct RawIdVisitor;

const INT_LEN: usize = size_of::<ShortId>();

impl RawId {
	const BACKFILLED_LEN: usize = size_of::<ShortRoomId>() + INT_LEN + size_of::<i64>();
	const MAX_LEN: usize = Self::BACKFILLED_LEN;
	const NORMAL_LEN: usize = size_of::<ShortRoomId>() + size_of::<u64>();

	/// Checks whether two raw PDU keys belong to the same room.
	///
	/// Only the leading compact room identifier is compared. Timeline kind and
	/// event count do not affect the result.
	#[inline]
	#[must_use]
	pub fn is_room_eq(self, other: Self) -> bool { self.shortroomid() == other.shortroomid() }

	/// Decodes the timeline count stored in this raw key.
	///
	/// Normal and backfilled layouts are converted to the corresponding `Count`
	/// variant. The compact room identifier is ignored.
	#[inline]
	#[must_use]
	pub fn pdu_count(self) -> Count {
		let id: Id = self.into();
		id.count
	}

	/// Returns the encoded compact room identifier.
	///
	/// The bytes remain in big-endian database order. Use `Id::from` when a
	/// typed integer value is needed.
	#[inline]
	#[must_use]
	pub fn shortroomid(self) -> [u8; INT_LEN] {
		match self {
			| Self::Normal(raw) => raw[0..INT_LEN]
				.try_into()
				.expect("normal raw shortroomid array from slice"),
			| Self::Backfilled(raw) => raw[0..INT_LEN]
				.try_into()
				.expect("backfilled raw shortroomid array from slice"),
		}
	}

	/// Returns the encoded timeline count.
	///
	/// The bytes remain in big-endian database order. For backfilled keys the
	/// intervening zero marker is omitted.
	#[inline]
	#[must_use]
	pub fn count(self) -> [u8; INT_LEN] {
		match self {
			| Self::Normal(raw) => raw[INT_LEN..INT_LEN * 2]
				.try_into()
				.expect("normal raw indice array from slice"),
			| Self::Backfilled(raw) => raw[INT_LEN * 2..INT_LEN * 3]
				.try_into()
				.expect("backfilled raw indice array from slice"),
		}
	}

	/// Borrows the complete packed database key.
	///
	/// The returned slice is 16 bytes for a normal key and 24 bytes for a
	/// backfilled key. Its lifetime is tied to this value.
	#[inline]
	#[must_use]
	pub fn as_bytes(&self) -> &[u8] {
		match self {
			| Self::Normal(raw) => raw,
			| Self::Backfilled(raw) => raw,
		}
	}
}

impl fmt::Debug for RawId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let id: Id = (*self).into();
		write!(f, "{id:?}")
	}
}

impl<'de> Deserialize<'de> for RawId {
	#[inline]
	fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
		d.deserialize_bytes(RawIdVisitor)
	}
}

impl serde::de::Visitor<'_> for RawIdVisitor {
	type Value = RawId;

	fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("RawId byte array")
	}

	#[inline]
	fn visit_bytes<E>(self, buf: &[u8]) -> Result<RawId, E> { Ok(RawId::from(buf)) }
}

impl From<Id> for RawId {
	#[inline]
	fn from(id: Id) -> Self {
		const MAX_LEN: usize = RawId::MAX_LEN;
		type RawVec = ArrayVec<u8, MAX_LEN>;

		let mut vec = RawVec::new();
		vec.extend(id.shortroomid.to_be_bytes());
		id.count.debug_assert_valid();
		match id.count {
			| Count::Normal(count) => {
				vec.extend(count.to_be_bytes());
				Self::Normal(
					vec.as_ref()
						.try_into()
						.expect("RawVec into RawId::Normal"),
				)
			},
			| Count::Backfilled(count) => {
				vec.extend(0_u64.to_be_bytes());
				vec.extend(count.to_be_bytes());
				Self::Backfilled(
					vec.as_ref()
						.try_into()
						.expect("RawVec into RawId::Backfilled"),
				)
			},
		}
	}
}

impl From<&[u8]> for RawId {
	#[inline]
	fn from(id: &[u8]) -> Self {
		match id.len() {
			| Self::NORMAL_LEN => Self::Normal(
				id[0..Self::NORMAL_LEN]
					.try_into()
					.expect("normal RawId from [u8]"),
			),
			| Self::BACKFILLED_LEN => Self::Backfilled(
				id[0..Self::BACKFILLED_LEN]
					.try_into()
					.expect("backfilled RawId from [u8]"),
			),
			| _ => unimplemented!("unrecognized RawId length"),
		}
	}
}

impl From<&[u8; Self::NORMAL_LEN]> for RawId {
	#[inline]
	fn from(id: &[u8; Self::NORMAL_LEN]) -> Self { Self::Normal(*id) }
}

impl From<&[u8; Self::BACKFILLED_LEN]> for RawId {
	#[inline]
	fn from(id: &[u8; Self::BACKFILLED_LEN]) -> Self { Self::Backfilled(*id) }
}

impl AsRef<[u8]> for RawId {
	#[inline]
	fn as_ref(&self) -> &[u8] { self.as_bytes() }
}
