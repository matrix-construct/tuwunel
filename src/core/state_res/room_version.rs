use ruma::RoomVersionId;

use super::{Error, Result};

#[derive(Debug)]
#[allow(clippy::exhaustive_enums)]
pub enum RoomDisposition {
	/// A room version that has a stable specification.
	Stable,
	/// A room version that is not yet fully specified.
	Unstable,
}

#[derive(Debug)]
#[cfg_attr(not(feature = "unstable-exhaustive-types"), non_exhaustive)]
pub enum EventFormatVersion {
	/// $id:server event id format
	V1,
	/// MSC1659-style $hash event id format: introduced for room v3
	V2,
	/// MSC1884-style $hash format: introduced for room v4
	V3,
}

#[derive(Debug)]
#[cfg_attr(not(feature = "unstable-exhaustive-types"), non_exhaustive)]
pub enum StateResolutionVersion {
	/// State resolution for rooms at version 1.
	V1,
	/// State resolution for room at version 2 or later.
	V2,
}

#[cfg_attr(not(feature = "unstable-exhaustive-types"), non_exhaustive)]
#[allow(clippy::struct_excessive_bools)]
pub struct RoomVersion {
	/// The stability of this room.
	pub disposition: RoomDisposition,
	/// The format of the EventId.
	pub event_format: EventFormatVersion,
	/// Which state resolution algorithm is used.
	pub state_res: StateResolutionVersion,
	// FIXME: not sure what this one means?
	pub enforce_key_validity: bool,

	/// `m.room.aliases` had special auth rules and redaction rules
	/// before room version 6.
	///
	/// before MSC2261/MSC2432,
	pub special_case_aliases_auth: bool,
	/// Strictly enforce canonical json, do not allow:
	/// * Integers outside the range of [-2 ^ 53 + 1, 2 ^ 53 - 1]
	/// * Floats
	/// * NaN, Infinity, -Infinity
	pub strict_canonicaljson: bool,
	/// Verify notifications key while checking m.room.power_levels.
	///
	/// bool: MSC2209: Check 'notifications'
	pub limit_notifications_power_levels: bool,
	/// Extra rules when verifying redaction events.
	pub extra_redaction_checks: bool,
	/// Allow knocking in event authentication.
	///
	/// See [room v7 specification](https://spec.matrix.org/latest/rooms/v7/) for more information.
	pub allow_knocking: bool,
	/// Adds support for the restricted join rule.
	///
	/// See: [MSC3289](https://github.com/matrix-org/matrix-spec-proposals/pull/3289) for more information.
	pub restricted_join_rules: bool,
	/// Adds support for the knock_restricted join rule.
	///
	/// See: [MSC3787](https://github.com/matrix-org/matrix-spec-proposals/pull/3787) for more information.
	pub knock_restricted_join_rule: bool,
	/// Enforces integer power levels.
	///
	/// See: [MSC3667](https://github.com/matrix-org/matrix-spec-proposals/pull/3667) for more information.
	pub integer_power_levels: bool,
	/// Determine the room creator using the `m.room.create` event's `sender`,
	/// instead of the event content's `creator` field.
	///
	/// See: [MSC2175](https://github.com/matrix-org/matrix-spec-proposals/pull/2175) for more information.
	pub use_room_create_sender: bool,
}

impl RoomVersion {
	pub const V1: Self = Self {
		disposition: RoomDisposition::Stable,
		event_format: EventFormatVersion::V1,
		state_res: StateResolutionVersion::V1,
		enforce_key_validity: false,
		special_case_aliases_auth: true,
		strict_canonicaljson: false,
		limit_notifications_power_levels: false,
		extra_redaction_checks: true,
		allow_knocking: false,
		restricted_join_rules: false,
		knock_restricted_join_rule: false,
		integer_power_levels: false,
		use_room_create_sender: false,
	};
	pub const V10: Self = Self {
		knock_restricted_join_rule: true,
		integer_power_levels: true,
		..Self::V9
	};
	pub const V11: Self = Self {
		use_room_create_sender: true,
		..Self::V10
	};
	pub const V2: Self = Self {
		state_res: StateResolutionVersion::V2,
		..Self::V1
	};
	pub const V3: Self = Self {
		event_format: EventFormatVersion::V2,
		extra_redaction_checks: false,
		..Self::V2
	};
	pub const V4: Self = Self {
		event_format: EventFormatVersion::V3,
		..Self::V3
	};
	pub const V5: Self = Self { enforce_key_validity: true, ..Self::V4 };
	pub const V6: Self = Self {
		special_case_aliases_auth: false,
		strict_canonicaljson: true,
		limit_notifications_power_levels: true,
		..Self::V5
	};
	pub const V7: Self = Self { allow_knocking: true, ..Self::V6 };
	pub const V8: Self = Self { restricted_join_rules: true, ..Self::V7 };
	pub const V9: Self = Self::V8;

	pub fn new(version: &RoomVersionId) -> Result<Self> {
		Ok(match version {
			| RoomVersionId::V1 => Self::V1,
			| RoomVersionId::V2 => Self::V2,
			| RoomVersionId::V3 => Self::V3,
			| RoomVersionId::V4 => Self::V4,
			| RoomVersionId::V5 => Self::V5,
			| RoomVersionId::V6 => Self::V6,
			| RoomVersionId::V7 => Self::V7,
			| RoomVersionId::V8 => Self::V8,
			| RoomVersionId::V9 => Self::V9,
			| RoomVersionId::V10 => Self::V10,
			| RoomVersionId::V11 => Self::V11,
			| ver => return Err(Error::Unsupported(format!("found version `{ver}`"))),
		})
	}
}
