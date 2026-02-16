//! Types to deserialize `m.room.member` events.

use std::ops::Deref;

use ruma::{
	CanonicalJsonObject, OwnedUserId, events::room::member::MembershipState,
	serde::from_raw_json_value, signatures::canonical_json,
};
use serde::Deserialize;
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{Err, Error, Result, debug_error, err, matrix::Event};

/// A helper type for an [`Event`] of type `m.room.member`.
///
/// This is a type that deserializes each field lazily, as requested.
#[derive(Debug, Clone)]
pub struct RoomMemberEvent<E: Event>(E);

impl<E: Event> RoomMemberEvent<E> {
	/// Construct a new `RoomMemberEvent` around the given event.
	#[inline]
	pub fn new(event: E) -> Self { Self(event) }

	/// The membership of the user.
	#[inline]
	pub fn membership(&self) -> Result<MembershipState> {
		RoomMemberEventContent(self.content()).membership()
	}

	/// If this is a `join` event, the ID of a user on the homeserver that
	/// authorized it.
	#[inline]
	pub fn join_authorised_via_users_server(&self) -> Result<Option<OwnedUserId>> {
		RoomMemberEventContent(self.content()).join_authorised_via_users_server()
	}

	/// If this is an `invite` event, details about the third-party invite that
	/// resulted in this event.
	#[inline]
	pub fn third_party_invite(&self) -> Result<Option<ThirdPartyInvite>> {
		RoomMemberEventContent(self.content()).third_party_invite()
	}
}

impl<E: Event> Deref for RoomMemberEvent<E> {
	type Target = E;

	#[inline]
	fn deref(&self) -> &Self::Target { &self.0 }
}

/// Helper trait for `Option<RoomMemberEvent<E>>`.
pub(crate) trait RoomMemberEventResultExt {
	/// The membership of the user.
	///
	/// Defaults to `leave` if there is no `m.room.member` event.
	fn membership(&self) -> Result<MembershipState>;
}

impl<E: Event> RoomMemberEventResultExt for Result<RoomMemberEvent<E>> {
	fn membership(&self) -> Result<MembershipState> {
		match self {
			| Ok(event) => event.membership(),
			| Err(e) if e.is_not_found() => Ok(MembershipState::Leave),
			| Err(e) if cfg!(test) => panic!("membership(): unexpected: {e}"),
			| Err(e) => {
				debug_error!("membership(): unexpected: {e}");
				Ok(MembershipState::Leave)
			},
		}
	}
}

/// A helper type for the raw JSON content of an event of type `m.room.member`.
pub struct RoomMemberEventContent<'a>(&'a RawJsonValue);

impl<'a> RoomMemberEventContent<'a> {
	/// Construct a new `RoomMemberEventContent` around the given raw JSON
	/// content.
	#[inline]
	#[must_use]
	pub fn new(content: &'a RawJsonValue) -> Self { Self(content) }
}

impl RoomMemberEventContent<'_> {
	/// The membership of the user.
	pub fn membership(&self) -> Result<MembershipState> {
		#[derive(Deserialize)]
		struct RoomMemberContentMembership {
			membership: MembershipState,
		}

		let content: RoomMemberContentMembership =
			from_raw_json_value(self.0).map_err(|err: Error| {
				err!(Request(InvalidParam(
					"missing or invalid `membership` field in `m.room.member` event: {err}"
				)))
			})?;

		Ok(content.membership)
	}

	/// If this is a `join` event, the ID of a user on the homeserver that
	/// authorized it.
	pub fn join_authorised_via_users_server(&self) -> Result<Option<OwnedUserId>> {
		#[derive(Deserialize)]
		struct RoomMemberContentJoinAuthorizedViaUsersServer {
			join_authorised_via_users_server: Option<OwnedUserId>,
		}

		let content: RoomMemberContentJoinAuthorizedViaUsersServer = from_raw_json_value(self.0)
			.map_err(|err: Error| {
				err!(Request(InvalidParam(
					"invalid `join_authorised_via_users_server` field in `m.room.member` event: \
					 {err}"
				)))
			})?;

		Ok(content.join_authorised_via_users_server)
	}

	/// If this is an `invite` event, details about the third-party invite that
	/// resulted in this event.
	pub fn third_party_invite(&self) -> Result<Option<ThirdPartyInvite>> {
		#[derive(Deserialize)]
		struct RoomMemberContentThirdPartyInvite {
			third_party_invite: Option<ThirdPartyInvite>,
		}

		let content: RoomMemberContentThirdPartyInvite =
			from_raw_json_value(self.0).map_err(|err: Error| {
				err!(Request(InvalidParam(
					"invalid `third_party_invite` field in `m.room.member` event: {err}"
				)))
			})?;

		Ok(content.third_party_invite)
	}
}

/// Details about a third-party invite.
#[derive(Deserialize)]
pub struct ThirdPartyInvite {
	/// Signed details about the third-party invite.
	signed: CanonicalJsonObject,
}

impl ThirdPartyInvite {
	/// The unique identifier for the third-party invite.
	pub fn token(&self) -> Result<&str> {
		let Some(token_value) = self.signed.get("token") else {
			return Err!(Request(InvalidParam(
				"missing `token` field in `third_party_invite.signed` of `m.room.member` event"
			)));
		};

		token_value.as_str().ok_or_else(|| {
			err!(Request(InvalidParam(
				"unexpected format of `token` field in `third_party_invite.signed` of \
				 `m.room.member` event: expected string, got {token_value:?}"
			)))
		})
	}

	/// The Matrix ID of the user that was invited.
	pub fn mxid(&self) -> Result<&str> {
		let Some(mxid_value) = self.signed.get("mxid") else {
			return Err!(Request(InvalidParam(
				"missing `mxid` field in `third_party_invite.signed` of `m.room.member` event"
			)));
		};

		mxid_value.as_str().ok_or_else(|| {
			err!(Request(InvalidParam(
				"unexpected format of `mxid` field in `third_party_invite.signed` of \
				 `m.room.member` event: expected string, got {mxid_value:?}"
			)))
		})
	}

	/// The signatures of the event.
	pub fn signatures(&self) -> Result<&CanonicalJsonObject> {
		let Some(signatures_value) = self.signed.get("signatures") else {
			return Err!(Request(InvalidParam(
				"missing `signatures` field in `third_party_invite.signed` of `m.room.member` \
				 event"
			)));
		};

		signatures_value.as_object().ok_or_else(|| {
			err!(Request(InvalidParam(
				"unexpected format of `signatures` field in `third_party_invite.signed` of \
				 `m.room.member` event: expected object, got {signatures_value:?}"
			)))
		})
	}

	/// The `signed` object as canonical JSON string to verify the signatures.
	pub fn signed_canonical_json(&self) -> Result<String> {
		canonical_json(&self.signed).map_err(|error| {
			err!(Request(InvalidParam(
				"invalid `third_party_invite.signed` field in `m.room.member` event: {error}"
			)))
		})
	}
}
