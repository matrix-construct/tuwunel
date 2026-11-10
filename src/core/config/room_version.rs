//! Defines supported Matrix room-version policy.
//!
//! Stable, unstable, and experimental version lists are kept here. [`Config`]
//! helpers apply the opt-in flags and report each enabled version's stability.

use ruma::{
	RoomVersionId,
	api::{
		client::discovery::get_capabilities::v3::RoomVersionStability,
		error::{ErrorKind, IncompatibleRoomVersionErrorData},
	},
};

use crate::{Config, Error, Result, implement};

/// Partially supported non-compliant room versions
pub const UNSTABLE_ROOM_VERSIONS: &[RoomVersionId] =
	&[RoomVersionId::V3, RoomVersionId::V4, RoomVersionId::V5];

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

/// Refuses a room version this configuration does not enable.
///
/// The refusal is `M_INCOMPATIBLE_ROOM_VERSION`, whose `room_version` field
/// names the refused version for the client or remote server.
#[implement(Config)]
pub fn require_supported_room_version(&self, version: &RoomVersionId) -> Result {
	if !self.supported_room_version(version) {
		let refused = IncompatibleRoomVersionErrorData::new(version.clone());

		return Err(Error::BadRequest(
			ErrorKind::IncompatibleRoomVersion(refused),
			"Room version not supported.",
		));
	}

	Ok(())
}

impl Config {
	#[inline]
	#[must_use]
	/// Reports whether a room version is enabled by this configuration.
	///
	/// Stable versions are always enabled. Unstable and experimental versions
	/// are included only when their respective configuration flags permit
	/// them.
	pub fn supported_room_version(&self, version: &RoomVersionId) -> bool {
		self.supported_room_versions()
			.any(|(supported_version, _)| &supported_version == version)
	}

	#[inline]
	/// Iterates over the room versions enabled by this configuration.
	///
	/// Stable versions appear first, followed by permitted unstable and
	/// experimental versions. Experimental entries are advertised as unstable.
	pub fn supported_room_versions(
		&self,
	) -> impl Iterator<Item = (RoomVersionId, RoomVersionStability)> + '_ {
		let stable_room_versions = STABLE_ROOM_VERSIONS
			.iter()
			.cloned()
			.map(|version| (version, RoomVersionStability::Stable));

		let unstable_room_versions = UNSTABLE_ROOM_VERSIONS
			.iter()
			.filter(|_| self.rooms.allow_unstable_versions)
			.cloned()
			.map(|version| (version, RoomVersionStability::Unstable));

		let experimental_room_versions = EXPERIMENTAL_ROOM_VERSIONS
			.iter()
			.filter(|_| self.rooms.allow_experimental_versions)
			.cloned()
			.map(|version| (version, RoomVersionStability::Unstable));

		stable_room_versions
			.chain(unstable_room_versions)
			.chain(experimental_room_versions)
	}
}
