//! Room version support

use std::iter::once;

use ruma::{RoomVersionId, api::client::discovery::get_capabilities::v3::RoomVersionStability};

use crate::{at, is_equal_to};

/// Partially supported non-compliant room versions
pub const UNSTABLE_ROOM_VERSIONS: &[RoomVersionId] =
	&[RoomVersionId::V2, RoomVersionId::V3, RoomVersionId::V4, RoomVersionId::V5];

/// Supported and stable room versions
pub const STABLE_ROOM_VERSIONS: &[RoomVersionId] = &[
	RoomVersionId::V6,
	RoomVersionId::V7,
	RoomVersionId::V8,
	RoomVersionId::V9,
	RoomVersionId::V10,
	RoomVersionId::V11,
	RoomVersionId::V12,
];

/// Experimental and prototype room versions under development.
pub const EXPERIMENTAL_ROOM_VERSIONS: &[RoomVersionId] = &[RoomVersionId::MSC4361];

type RoomVersion = (RoomVersionId, RoomVersionStability);

impl crate::Server {
	#[inline]
	pub fn supported_room_version(&self, version: &RoomVersionId) -> bool {
		self.supported_room_versions()
			.any(is_equal_to!(*version))
	}

	#[inline]
	pub fn supported_room_versions(&self) -> impl Iterator<Item = RoomVersionId> + '_ {
		let experimental_room_versions = EXPERIMENTAL_ROOM_VERSIONS
			.iter()
			.cloned()
			.zip(once(RoomVersionStability::Unstable).cycle())
			.filter(|_| self.config.allow_experimental_room_versions);

		Self::available_room_versions()
			.filter(|(_, stability)| self.supported_stability(stability))
			.chain(experimental_room_versions)
			.map(at!(0))
	}

	#[inline]
	pub fn available_room_versions() -> impl Iterator<Item = RoomVersion> {
		available_room_versions()
	}

	#[inline]
	fn supported_stability(&self, stability: &RoomVersionStability) -> bool {
		self.config.allow_unstable_room_versions || *stability == RoomVersionStability::Stable
	}
}

pub fn available_room_versions() -> impl Iterator<Item = RoomVersion> {
	let unstable_room_versions = UNSTABLE_ROOM_VERSIONS
		.iter()
		.cloned()
		.zip(once(RoomVersionStability::Unstable).cycle());

	STABLE_ROOM_VERSIONS
		.iter()
		.cloned()
		.zip(once(RoomVersionStability::Stable).cycle())
		.chain(unstable_room_versions)
}
