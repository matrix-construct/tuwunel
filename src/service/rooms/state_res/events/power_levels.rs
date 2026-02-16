//! Types to deserialize `m.room.power_levels` events.

use std::ops::Deref;

use ruma::{
	Int, OwnedUserId, UserId,
	events::{TimelineEventType, room::power_levels::UserPowerLevel},
	int,
	room_version_rules::AuthorizationRules,
	serde::{
		DebugAsRefStr, DisplayAsRefStr, JsonObject, OrdAsRefStr, PartialEqAsRefStr,
		PartialOrdAsRefStr, deserialize_v1_powerlevel, from_raw_json_value,
		vec_deserialize_int_powerlevel_values, vec_deserialize_v1_powerlevel_values,
	},
};
use serde::de::DeserializeOwned;
use serde_json::{Error, from_value as from_json_value};
use tuwunel_core::{Result, err, is_equal_to, matrix::Event, ref_at};

/// The default value of the creator's power level.
const DEFAULT_CREATOR_POWER_LEVEL: i32 = 100;

/// A helper type for an [`Event`] of type `m.room.power_levels`.
#[derive(Clone, Debug)]
pub struct RoomPowerLevelsEvent<E: Event>(E);

impl<E: Event> RoomPowerLevelsEvent<E> {
	/// Construct a new `RoomPowerLevelsEvent` around the given event.
	#[inline]
	pub fn new(event: E) -> Self { Self(event) }

	/// The deserialized content of the event.
	fn deserialized_content(&self) -> Result<JsonObject> {
		from_raw_json_value(self.content()).map_err(|error: Error| {
			err!(Request(InvalidParam("malformed `m.room.power_levels` content: {error}")))
		})
	}

	/// Get the value of a field that should contain an integer, if any.
	///
	/// The deserialization of this field is cached in memory.
	pub(crate) fn get_as_int(
		&self,
		field: RoomPowerLevelsIntField,
		rules: &AuthorizationRules,
	) -> Result<Option<Int>> {
		let content = self.deserialized_content()?;

		let Some(value) = content.get(field.as_str()) else {
			return Ok(None);
		};

		let res = if rules.integer_power_levels {
			from_json_value(value.clone())
		} else {
			deserialize_v1_powerlevel(value)
		};

		let power_level = res.map(Some).map_err(|error| {
			err!(Request(InvalidParam(
				"unexpected format of `{field}` field in `content` of `m.room.power_levels` \
				 event: {error}"
			)))
		})?;

		Ok(power_level)
	}

	/// Get the value of a field that should contain an integer, or its default
	/// value if it is absent.
	#[inline]
	pub(crate) fn get_as_int_or_default(
		&self,
		field: RoomPowerLevelsIntField,
		rules: &AuthorizationRules,
	) -> Result<Int> {
		Ok(self
			.get_as_int(field, rules)?
			.unwrap_or_else(|| field.default_value()))
	}

	/// Get the value of a field that should contain a map of any value to
	/// integer, if any.
	fn get_as_int_map<T: Ord + DeserializeOwned>(
		&self,
		field: &str,
		rules: &AuthorizationRules,
	) -> Result<Option<Vec<(T, Int)>>> {
		let content = self.deserialized_content()?;

		let Some(value) = content.get(field) else {
			return Ok(None);
		};

		let res = if rules.integer_power_levels {
			vec_deserialize_int_powerlevel_values(value)
		} else {
			vec_deserialize_v1_powerlevel_values(value)
		};

		res.map(Some).map_err(|error| {
			err!(Request(InvalidParam(
				"unexpected format of `{field}` field in `content` of `m.room.power_levels` \
				 event: {error}"
			)))
		})
	}

	/// Get the power levels required to send events, if any.
	#[inline]
	pub(crate) fn events(
		&self,
		rules: &AuthorizationRules,
	) -> Result<Option<Vec<(TimelineEventType, Int)>>> {
		self.get_as_int_map("events", rules)
	}

	/// Get the power levels required to trigger notifications, if any.
	#[inline]
	pub(crate) fn notifications(
		&self,
		rules: &AuthorizationRules,
	) -> Result<Option<Vec<(String, Int)>>> {
		self.get_as_int_map("notifications", rules)
	}

	/// Get the power levels of the users, if any.
	///
	/// The deserialization of this field is cached in memory.
	#[inline]
	pub(crate) fn users(
		&self,
		rules: &AuthorizationRules,
	) -> Result<Option<Vec<(OwnedUserId, Int)>>> {
		self.get_as_int_map("users", rules)
	}

	/// Get the power level of the user with the given ID.
	///
	/// Calling this method several times should be cheap because the necessary
	/// deserialization results are cached.
	pub(crate) fn user_power_level(
		&self,
		user_id: &UserId,
		rules: &AuthorizationRules,
	) -> Result<UserPowerLevel> {
		let power_level = if let Some(power_level) = self
			.users(rules)?
			.as_ref()
			.and_then(|users| get_value(users, user_id))
		{
			Ok(*power_level)
		} else {
			self.get_as_int_or_default(RoomPowerLevelsIntField::UsersDefault, rules)
		};

		power_level.map(Into::into)
	}

	/// Get the power level required to send an event of the given type.
	pub(crate) fn event_power_level(
		&self,
		event_type: &TimelineEventType,
		state_key: Option<&str>,
		rules: &AuthorizationRules,
	) -> Result<Int> {
		let events = self.events(rules)?;

		if let Some(power_level) = events
			.as_ref()
			.and_then(|events| get_value(events, event_type))
		{
			return Ok(*power_level);
		}

		let default_field = if state_key.is_some() {
			RoomPowerLevelsIntField::StateDefault
		} else {
			RoomPowerLevelsIntField::EventsDefault
		};

		self.get_as_int_or_default(default_field, rules)
	}

	/// Get a map of all the fields with an integer value in the `content` of an
	/// `m.room.power_levels` event.
	pub(crate) fn int_fields_map(
		&self,
		rules: &AuthorizationRules,
	) -> Result<Vec<(RoomPowerLevelsIntField, Int)>> {
		RoomPowerLevelsIntField::ALL
			.iter()
			.copied()
			.filter_map(|field| match self.get_as_int(field, rules) {
				| Ok(value) => value.map(|value| Ok((field, value))),
				| Err(error) => Some(Err(error)),
			})
			.collect()
	}
}

impl<E: Event> Deref for RoomPowerLevelsEvent<E> {
	type Target = E;

	#[inline]
	fn deref(&self) -> &Self::Target { &self.0 }
}

/// Helper trait for `Option<RoomPowerLevelsEvent<E>>`.
pub(crate) trait RoomPowerLevelsEventOptionExt {
	/// Get the power level of the user with the given ID.
	fn user_power_level(
		&self,
		user_id: &UserId,
		creators: impl Iterator<Item = OwnedUserId>,
		rules: &AuthorizationRules,
	) -> Result<UserPowerLevel>;

	/// Get the value of a field that should contain an integer, or its default
	/// value if it is absent.
	fn get_as_int_or_default(
		&self,
		field: RoomPowerLevelsIntField,
		rules: &AuthorizationRules,
	) -> Result<Int>;

	/// Get the power level required to send an event of the given type.
	fn event_power_level(
		&self,
		event_type: &TimelineEventType,
		state_key: Option<&str>,
		rules: &AuthorizationRules,
	) -> Result<Int>;
}

impl<E> RoomPowerLevelsEventOptionExt for Option<RoomPowerLevelsEvent<E>>
where
	E: Event,
{
	fn user_power_level(
		&self,
		user_id: &UserId,
		mut creators: impl Iterator<Item = OwnedUserId>,
		rules: &AuthorizationRules,
	) -> Result<UserPowerLevel> {
		if rules.explicitly_privilege_room_creators && creators.any(is_equal_to!(user_id)) {
			Ok(UserPowerLevel::Infinite)
		} else if let Some(room_power_levels_event) = self {
			room_power_levels_event.user_power_level(user_id, rules)
		} else {
			let power_level = if creators.any(is_equal_to!(user_id)) {
				DEFAULT_CREATOR_POWER_LEVEL.into()
			} else {
				RoomPowerLevelsIntField::UsersDefault.default_value()
			};

			Ok(power_level.into())
		}
	}

	fn get_as_int_or_default(
		&self,
		field: RoomPowerLevelsIntField,
		rules: &AuthorizationRules,
	) -> Result<Int> {
		if let Some(room_power_levels_event) = self {
			room_power_levels_event.get_as_int_or_default(field, rules)
		} else {
			Ok(field.default_value())
		}
	}

	fn event_power_level(
		&self,
		event_type: &TimelineEventType,
		state_key: Option<&str>,
		rules: &AuthorizationRules,
	) -> Result<Int> {
		if let Some(room_power_levels_event) = self {
			room_power_levels_event.event_power_level(event_type, state_key, rules)
		} else {
			let default_field = if state_key.is_some() {
				RoomPowerLevelsIntField::StateDefault
			} else {
				RoomPowerLevelsIntField::EventsDefault
			};

			Ok(default_field.default_value())
		}
	}
}

#[inline]
pub(crate) fn get_value<'a, K, V, B>(vec: &'a [(K, V)], key: &'a B) -> Option<&'a V>
where
	&'a K: PartialEq<&'a B>,
	B: ?Sized,
{
	position(vec, key)
		.and_then(|i| vec.get(i))
		.map(ref_at!(1))
}

#[inline]
pub(crate) fn contains_key<'a, K, V, B>(vec: &'a [(K, V)], key: &'a B) -> bool
where
	&'a K: PartialEq<&'a B>,
	B: ?Sized,
{
	position(vec, key).is_some()
}

fn position<'a, K, V, B>(vec: &'a [(K, V)], key: &'a B) -> Option<usize>
where
	&'a K: PartialEq<&'a B>,
	B: ?Sized,
{
	vec.iter()
		.map(ref_at!(0))
		.position(is_equal_to!(key))
}

/// Fields in the `content` of an `m.room.power_levels` event with an integer
/// value.
#[derive(
	DebugAsRefStr,
	Clone,
	Copy,
	DisplayAsRefStr,
	PartialEqAsRefStr,
	Eq,
	PartialOrdAsRefStr,
	OrdAsRefStr,
)]
#[non_exhaustive]
pub enum RoomPowerLevelsIntField {
	/// `users_default`
	UsersDefault,

	/// `events_default`
	EventsDefault,

	/// `state_default`
	StateDefault,

	/// `ban`
	Ban,

	/// `redact`
	Redact,

	/// `kick`
	Kick,

	/// `invite`
	Invite,
}

impl RoomPowerLevelsIntField {
	/// A slice containing all the variants.
	pub const ALL: &[Self] = &[
		Self::UsersDefault,
		Self::EventsDefault,
		Self::StateDefault,
		Self::Ban,
		Self::Redact,
		Self::Kick,
		Self::Invite,
	];

	/// The string representation of this field.
	#[inline]
	#[must_use]
	pub fn as_str(&self) -> &str { self.as_ref() }

	/// The default value for this field if it is absent.
	#[inline]
	#[must_use]
	pub fn default_value(self) -> Int {
		match self {
			| Self::UsersDefault | Self::EventsDefault | Self::Invite => int!(0),
			| Self::StateDefault | Self::Kick | Self::Ban | Self::Redact => int!(50),
		}
	}
}

impl AsRef<str> for RoomPowerLevelsIntField {
	#[inline]
	fn as_ref(&self) -> &'static str {
		match self {
			| Self::UsersDefault => "users_default",
			| Self::EventsDefault => "events_default",
			| Self::StateDefault => "state_default",
			| Self::Ban => "ban",
			| Self::Redact => "redact",
			| Self::Kick => "kick",
			| Self::Invite => "invite",
		}
	}
}
