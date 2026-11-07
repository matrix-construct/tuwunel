//! Joined members against their rooms' current state.
//!
//! `roomuserid_joined` lists who is joined to each room, and the member event in
//! each room's current state says the same. A conduwuit-era bug could leave the
//! two disagreeing.

use std::{fmt, str::from_utf8};

use futures::Stream;
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId, events::room::member::MembershipState};
use tuwunel_core::{
	Result,
	utils::{TryReadyExt, stream::TryBroadbandExt},
};
use tuwunel_database::{KeyBuf, SEP};

use super::{Check, Report, ScanExt, tally, undecodable};
use crate::Services;

type Member = (OwnedRoomId, OwnedUserId);

pub(super) static CHECK: Check = Check {
	name: "joined-members",
	about: "Every member roomuserid_joined lists has a join in its room's current state",
	quiescent: true,
	walk: |services, limit| tally(scan(services), limit),
};

/// A joined member whose room's current state does not say joined.
///
/// The membership repair marks a contradicted member left, and leaves a member
/// the state does not resolve as it is.
pub enum Finding {
	/// The current state gives the member another membership.
	Contradicted {
		/// The room.
		room_id: OwnedRoomId,

		/// The member.
		user_id: OwnedUserId,

		/// The membership the current state gives.
		membership: MembershipState,
	},

	/// The current state holds no member event for the member.
	Unbacked {
		/// The room.
		room_id: OwnedRoomId,

		/// The member.
		user_id: OwnedUserId,
	},

	/// The member event in the current state does not decode.
	Unreadable {
		/// The room.
		room_id: OwnedRoomId,

		/// The member.
		user_id: OwnedUserId,
	},

	/// The key separates its ids, but they do not read as a room and a user.
	Unparsable {
		/// The stored key.
		key: KeyBuf,
	},
}

/// Streams each joined member the current state does not show joined.
///
/// Every row of `roomuserid_joined` is read and its member looked up in the
/// room's current state, as the membership repair walks them. A key whose first
/// separator is missing or doubled is skipped, as the joined keys check reports
/// it.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	services.db["roomuserid_joined"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|(key, _)| Ok(split(key)))
		.broad_and_then(async |member| match member {
			| Ok(member) => verify(services, member).await,
			| Err(key) => Ok(Some(Finding::Unparsable { key })),
		})
		.ready_try_filter_map(Ok)
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Contradicted { .. } => "contradicted-join",
			| Self::Unbacked { .. } => "unbacked-join",
			| Self::Unreadable { .. } => "unreadable-member",
			| Self::Unparsable { .. } => "unparsable-key",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Contradicted { room_id, user_id, membership } => {
				write!(
					f,
					"{user_id} is listed joined to {room_id}, whose state says {membership}"
				)
			},
			| Self::Unbacked { room_id, user_id } => {
				write!(
					f,
					"{user_id} is listed joined to {room_id}, whose state has no member event"
				)
			},
			| Self::Unreadable { room_id, user_id } => {
				write!(f, "member event of {user_id} in {room_id} does not decode")
			},
			| Self::Unparsable { key } => {
				write!(f, "key does not read as a room and a user: {}", key.escape_ascii())
			},
		}
	}
}

/// Splits a key into its room and user, by hand.
///
/// A key the joined keys check reports gives `None`, and one whose ids do not
/// parse gives the key back.
fn split(key: &[u8]) -> Option<Result<Member, KeyBuf>> {
	let at = key.iter().position(|&byte| byte == SEP)?;
	let (room_id, user_id) = key.split_at(at);

	user_id
		.strip_prefix(&[SEP])
		.filter(|user_id| user_id.first() != Some(&SEP))
		.map(|user_id| member(room_id, user_id).ok_or_else(|| KeyBuf::from_slice(key)))
}

fn member(room_id: &[u8], user_id: &[u8]) -> Option<Member> {
	let room_id: &RoomId = from_utf8(room_id).ok()?.try_into().ok()?;
	let user_id: &UserId = from_utf8(user_id).ok()?.try_into().ok()?;

	Some((room_id.to_owned(), user_id.to_owned()))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn verify(services: &Services, (room_id, user_id): Member) -> Result<Option<Finding>> {
	let member = services
		.state_accessor
		.get_member(&room_id, &user_id)
		.await;

	let finding: Option<Finding> = match member {
		| Ok(member) if member.membership == MembershipState::Join => None,
		| Ok(member) => Finding::Contradicted {
			room_id,
			user_id,
			membership: member.membership,
		}
		.into(),
		| Err(error) if error.is_not_found() => Some(Finding::Unbacked { room_id, user_id }),
		| Err(error) if undecodable(&error) => Some(Finding::Unreadable { room_id, user_id }),
		| Err(error) => return Err(error),
	};

	Ok(finding)
}
