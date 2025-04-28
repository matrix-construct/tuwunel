use arrayvec::ArrayVec;

use super::{Count, Id, ShortEventId, ShortId, ShortRoomId};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RawId {
	Normal(RawIdNormal),
	Backfilled(RawIdBackfilled),
}

type RawIdNormal = [u8; RawId::NORMAL_LEN];
type RawIdBackfilled = [u8; RawId::BACKFILLED_LEN];

const INT_LEN: usize = size_of::<ShortId>();

impl RawId {
	const BACKFILLED_LEN: usize = size_of::<ShortRoomId>() + INT_LEN + size_of::<ShortEventId>();
	const MAX_LEN: usize = Self::BACKFILLED_LEN;
	const NORMAL_LEN: usize = size_of::<ShortRoomId>() + size_of::<ShortEventId>();

	#[inline]
	#[must_use]
	pub fn pdu_count(&self) -> Count {
		let id: Id = (*self).into();
		id.shorteventid
	}

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

	#[inline]
	#[must_use]
	pub fn shorteventid(self) -> [u8; INT_LEN] {
		match self {
			| Self::Normal(raw) => raw[INT_LEN..INT_LEN * 2]
				.try_into()
				.expect("normal raw shorteventid array from slice"),
			| Self::Backfilled(raw) => raw[INT_LEN * 2..INT_LEN * 3]
				.try_into()
				.expect("backfilled raw shorteventid array from slice"),
		}
	}

	#[inline]
	#[must_use]
	pub fn as_bytes(&self) -> &[u8] {
		match self {
			| Self::Normal(raw) => raw,
			| Self::Backfilled(raw) => raw,
		}
	}
}

impl AsRef<[u8]> for RawId {
	#[inline]
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
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

impl From<Id> for RawId {
	#[inline]
	fn from(id: Id) -> Self {
		const MAX_LEN: usize = RawId::MAX_LEN;
		type RawVec = ArrayVec<u8, MAX_LEN>;

		let mut vec = RawVec::new();
		vec.extend(id.shortroomid.to_be_bytes());
		id.shorteventid.debug_assert_valid();
		match id.shorteventid {
			| Count::Normal(shorteventid) => {
				vec.extend(shorteventid.to_be_bytes());
				Self::Normal(
					vec.as_ref()
						.try_into()
						.expect("RawVec into RawId::Normal"),
				)
			},
			| Count::Backfilled(shorteventid) => {
				vec.extend(0_u64.to_be_bytes());
				vec.extend(shorteventid.to_be_bytes());
				Self::Backfilled(
					vec.as_ref()
						.try_into()
						.expect("RawVec into RawId::Backfilled"),
				)
			},
		}
	}
}
