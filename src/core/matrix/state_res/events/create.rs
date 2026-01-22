//! Types to deserialize `m.room.create` events.

use std::{borrow::Cow, iter, ops::Deref};

use ruma::{
	OwnedUserId, RoomVersionId, UserId, room_version_rules::AuthorizationRules,
	serde::from_raw_json_value,
};
use serde::{Deserialize, de::IgnoredAny};

use crate::{Error, Result, err, matrix::Event};

/// A helper type for an [`Event`] of type `m.room.create`.
///
/// This is a type that deserializes each field lazily, when requested.
#[derive(Debug)]
pub struct RoomCreateEvent<E: Event>(E);

impl<E: Event> RoomCreateEvent<E> {
	/// Construct a new `RoomCreateEvent` around the given event.
	#[inline]
	pub fn new(event: E) -> Self { Self(event) }

	/// The version of the room.
	pub fn room_version(&self) -> Result<RoomVersionId> {
		#[derive(Deserialize)]
		struct RoomCreateContentRoomVersion {
			room_version: Option<RoomVersionId>,
		}

		let content: RoomCreateContentRoomVersion =
			from_raw_json_value(self.content()).map_err(|err: Error| {
				err!("invalid `room_version` field in `m.room.create` event: {err}")
			})?;

		Ok(content.room_version.unwrap_or(RoomVersionId::V1))
	}

	/// Whether the room is federated.
	pub fn federate(&self) -> Result<bool> {
		#[derive(Deserialize)]
		struct RoomCreateContentFederate {
			#[serde(rename = "m.federate")]
			federate: Option<bool>,
		}

		let content: RoomCreateContentFederate =
			from_raw_json_value(self.content()).map_err(|err: Error| {
				err!("invalid `m.federate` field in `m.room.create` event: {err}")
			})?;

		Ok(content.federate.unwrap_or(true))
	}

	/// The creator of the room.
	///
	/// If the `use_room_create_sender` field of `AuthorizationRules` is set,
	/// the creator is the sender of this `m.room.create` event, otherwise it
	/// is deserialized from the `creator` field of this event's content.
	pub fn creator(&self, rules: &AuthorizationRules) -> Result<Cow<'_, UserId>> {
		#[derive(Deserialize)]
		struct RoomCreateContentCreator {
			creator: OwnedUserId,
		}

		if rules.use_room_create_sender {
			Ok(Cow::Borrowed(self.sender()))
		} else {
			let content: RoomCreateContentCreator =
				from_raw_json_value(self.content()).map_err(|err: Error| {
					err!("missing or invalid `creator` field in `m.room.create` event: {err}")
				})?;

			Ok(Cow::Owned(content.creator))
		}
	}

	/// The creators of the room.
	///
	/// If the `use_room_create_sender` field of `AuthorizationRules` is set,
	/// the creator is the sender of this `m.room.create` event, otherwise it
	/// is deserialized from the `creator` field of this event's content.
	/// Additionally if the `explicitly_privilege_room_creators`
	/// field of `AuthorizationRules` is set, any additional user IDs in
	/// `additional_creators`, if present, will also be considered creators.
	pub fn creators<'a>(
		&'a self,
		rules: &'a AuthorizationRules,
	) -> Result<impl Iterator<Item = OwnedUserId> + Clone + use<'a, E>> {
		let initial = self.creator(rules)?.into_owned();
		let additional = self.additional_creators(rules)?;

		Ok(iter::once(initial).chain(additional))
	}

	/// The additional creators of the room (if any).
	///
	/// If the `explicitly_privilege_room_creators`
	/// field of `AuthorizationRules` is set, any additional user IDs in
	/// `additional_creators`, if present, will also be considered creators.
	///
	/// This function ignores the primary room creator, and should only be used
	/// in `check_room_member_join`. Otherwise, you should use `creators`
	/// instead.
	pub(super) fn additional_creators(
		&self,
		rules: &AuthorizationRules,
	) -> Result<impl Iterator<Item = OwnedUserId> + Clone> {
		#[derive(Deserialize)]
		struct RoomCreateContentAdditionalCreators {
			#[serde(default)]
			additional_creators: Vec<OwnedUserId>,
		}

		Ok(if rules.additional_room_creators {
			let mut content: RoomCreateContentAdditionalCreators =
				from_raw_json_value(self.content()).map_err(|err: serde_json::Error| {
					err!("invalid `additional_creators` field in `m.room.create` event: {err}")
				})?;

			content.additional_creators.sort();
			content.additional_creators.dedup();
			content.additional_creators.into_iter()
		} else {
			Vec::new().into_iter()
		})
	}

	/// Whether the event has a `creator` field.
	pub fn has_creator(&self) -> Result<bool> {
		#[derive(Deserialize)]
		struct RoomCreateContentCreator {
			creator: Option<IgnoredAny>,
		}

		let content: RoomCreateContentCreator =
			from_raw_json_value(self.content()).map_err(|err: Error| {
				err!("invalid `creator` field in `m.room.create` event: {err}")
			})?;

		Ok(content.creator.is_some())
	}
}

impl<E: Event> Deref for RoomCreateEvent<E> {
	type Target = E;

	#[inline]
	fn deref(&self) -> &Self::Target { &self.0 }
}
