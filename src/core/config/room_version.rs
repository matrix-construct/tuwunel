use ruma::{RoomVersionId, api::client::discovery::get_capabilities::v3::RoomVersionStability};

use crate::Config;

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
pub const EXPERIMENTAL_ROOM_VERSIONS: &[RoomVersionId] = &[];

impl Config {
	#[inline]
	#[must_use]
	pub fn supported_room_version(&self, version: &RoomVersionId) -> bool {
		self.supported_room_versions()
			.any(|(supported_version, _)| &supported_version == version)
	}

	#[inline]
	pub fn supported_room_versions(
		&self,
	) -> impl Iterator<Item = (RoomVersionId, RoomVersionStability)> + '_ {
		let stable_room_versions = STABLE_ROOM_VERSIONS
			.iter()
			.cloned()
			.map(|version| (version, RoomVersionStability::Stable));

		let unstable_room_versions = UNSTABLE_ROOM_VERSIONS
			.iter()
			.filter(|_| self.allow_unstable_room_versions)
			.cloned()
			.map(|version| (version, RoomVersionStability::Unstable));

		let experimental_room_versions = EXPERIMENTAL_ROOM_VERSIONS
			.iter()
			.filter(|_| self.allow_experimental_room_versions)
			.cloned()
			.map(|version| (version, RoomVersionStability::Unstable));

		stable_room_versions
			.chain(unstable_room_versions)
			.chain(experimental_room_versions)
	}
}
