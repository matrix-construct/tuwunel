use std::collections::BTreeMap;

use ruma::{
	Int, OwnedUserId, UserId,
	events::{TimelineEventType, room::power_levels::RoomPowerLevelsEventContent},
	power_levels::{NotificationPowerLevels, default_power_level},
	serde::{
		deserialize_v1_powerlevel, vec_deserialize_int_powerlevel_values,
		vec_deserialize_v1_powerlevel_values,
	},
};
use serde::Deserialize;
use serde_json::{Error, from_str as from_json_str};

use super::{Result, RoomVersion};
use crate::error;

#[derive(Deserialize)]
struct IntRoomPowerLevelsEventContent {
	#[serde(default = "default_power_level")]
	ban: Int,

	#[serde(default)]
	events: BTreeMap<TimelineEventType, Int>,

	#[serde(default)]
	events_default: Int,

	#[serde(default)]
	invite: Int,

	#[serde(default = "default_power_level")]
	kick: Int,

	#[serde(default = "default_power_level")]
	redact: Int,

	#[serde(default = "default_power_level")]
	state_default: Int,

	#[serde(default)]
	users: BTreeMap<OwnedUserId, Int>,

	#[serde(default)]
	users_default: Int,

	#[serde(default)]
	notifications: IntNotificationPowerLevels,
}

impl From<IntRoomPowerLevelsEventContent> for RoomPowerLevelsEventContent {
	fn from(int_pl: IntRoomPowerLevelsEventContent) -> Self {
		let IntRoomPowerLevelsEventContent {
			ban,
			events,
			events_default,
			invite,
			kick,
			redact,
			state_default,
			users,
			users_default,
			notifications,
		} = int_pl;

		let mut pl = Self::new();
		pl.ban = ban;
		pl.events = events;
		pl.events_default = events_default;
		pl.invite = invite;
		pl.kick = kick;
		pl.redact = redact;
		pl.state_default = state_default;
		pl.users = users;
		pl.users_default = users_default;
		pl.notifications = notifications.into();

		pl
	}
}

#[derive(Deserialize)]
struct IntNotificationPowerLevels {
	#[serde(default = "default_power_level")]
	room: Int,
}

impl Default for IntNotificationPowerLevels {
	fn default() -> Self {
		Self { room: default_power_level() }
	}
}

impl From<IntNotificationPowerLevels> for NotificationPowerLevels {
	fn from(int_notif: IntNotificationPowerLevels) -> Self {
		let mut notif = Self::new();
		notif.room = int_notif.room;

		notif
	}
}

#[inline]
pub(crate) fn deserialize_power_levels(
	content: &str,
	room_version: &RoomVersion,
) -> Option<RoomPowerLevelsEventContent> {
	if room_version.integer_power_levels {
		deserialize_integer_power_levels(content)
	} else {
		deserialize_legacy_power_levels(content)
	}
}

fn deserialize_integer_power_levels(content: &str) -> Option<RoomPowerLevelsEventContent> {
	match from_json_str::<IntRoomPowerLevelsEventContent>(content) {
		| Ok(content) => Some(content.into()),
		| Err(_) => {
			error!("m.room.power_levels event is not valid with integer values");
			None
		},
	}
}

fn deserialize_legacy_power_levels(content: &str) -> Option<RoomPowerLevelsEventContent> {
	match from_json_str(content) {
		| Ok(content) => Some(content),
		| Err(_) => {
			error!(
				"m.room.power_levels event is not valid with integer or string integer values"
			);
			None
		},
	}
}

#[derive(Deserialize)]
pub(crate) struct PowerLevelsContentFields {
	#[serde(default, deserialize_with = "vec_deserialize_v1_powerlevel_values")]
	pub(crate) users: Vec<(OwnedUserId, Int)>,

	#[serde(default, deserialize_with = "deserialize_v1_powerlevel")]
	pub(crate) users_default: Int,
}

impl PowerLevelsContentFields {
	pub(crate) fn get_user_power(&self, user_id: &UserId) -> Option<&Int> {
		let comparator = |item: &(OwnedUserId, Int)| {
			let item: &UserId = &item.0;
			item.cmp(user_id)
		};

		self.users
			.binary_search_by(comparator)
			.ok()
			.and_then(|idx| self.users.get(idx).map(|item| &item.1))
	}
}

#[derive(Deserialize)]
struct IntPowerLevelsContentFields {
	#[serde(default, deserialize_with = "vec_deserialize_int_powerlevel_values")]
	users: Vec<(OwnedUserId, Int)>,

	#[serde(default)]
	users_default: Int,
}

impl From<IntPowerLevelsContentFields> for PowerLevelsContentFields {
	fn from(pl: IntPowerLevelsContentFields) -> Self {
		let IntPowerLevelsContentFields { users, users_default } = pl;
		Self { users, users_default }
	}
}

#[inline]
pub(crate) fn deserialize_power_levels_content_fields(
	content: &str,
	room_version: &RoomVersion,
) -> Result<PowerLevelsContentFields, Error> {
	if room_version.integer_power_levels {
		deserialize_integer_power_levels_content_fields(content)
	} else {
		deserialize_legacy_power_levels_content_fields(content)
	}
}

fn deserialize_integer_power_levels_content_fields(
	content: &str,
) -> Result<PowerLevelsContentFields, Error> {
	from_json_str::<IntPowerLevelsContentFields>(content).map(Into::into)
}

fn deserialize_legacy_power_levels_content_fields(
	content: &str,
) -> Result<PowerLevelsContentFields, Error> {
	from_json_str(content)
}

#[derive(Deserialize)]
pub(crate) struct PowerLevelsContentInvite {
	#[serde(default, deserialize_with = "deserialize_v1_powerlevel")]
	pub(crate) invite: Int,
}

#[derive(Deserialize)]
struct IntPowerLevelsContentInvite {
	#[serde(default)]
	invite: Int,
}

impl From<IntPowerLevelsContentInvite> for PowerLevelsContentInvite {
	fn from(pl: IntPowerLevelsContentInvite) -> Self {
		let IntPowerLevelsContentInvite { invite } = pl;
		Self { invite }
	}
}

pub(crate) fn deserialize_power_levels_content_invite(
	content: &str,
	room_version: &RoomVersion,
) -> Result<PowerLevelsContentInvite, Error> {
	if room_version.integer_power_levels {
		from_json_str::<IntPowerLevelsContentInvite>(content).map(Into::into)
	} else {
		from_json_str(content)
	}
}

#[derive(Deserialize)]
pub(crate) struct PowerLevelsContentRedact {
	#[serde(default = "default_power_level", deserialize_with = "deserialize_v1_powerlevel")]
	pub(crate) redact: Int,
}

#[derive(Deserialize)]
pub(crate) struct IntPowerLevelsContentRedact {
	#[serde(default = "default_power_level")]
	redact: Int,
}

impl From<IntPowerLevelsContentRedact> for PowerLevelsContentRedact {
	fn from(pl: IntPowerLevelsContentRedact) -> Self {
		let IntPowerLevelsContentRedact { redact } = pl;
		Self { redact }
	}
}

pub(crate) fn deserialize_power_levels_content_redact(
	content: &str,
	room_version: &RoomVersion,
) -> Result<PowerLevelsContentRedact, Error> {
	if room_version.integer_power_levels {
		from_json_str::<IntPowerLevelsContentRedact>(content).map(Into::into)
	} else {
		from_json_str(content)
	}
}
