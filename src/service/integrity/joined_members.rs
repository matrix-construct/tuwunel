//! Joined members against their rooms' current state.
//!
//! `roomuserid_joined` lists who is joined to each room, and the member event in
//! each room's current state says the same. A conduwuit-era bug could leave the
//! two disagreeing.

use std::{fmt, str::from_utf8, sync::Arc};

use futures::{Stream, TryFutureExt};
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId, events::room::member::MembershipState};
use tuwunel_core::{
	Result,
	result::NotFound,
	utils::{TryReadyExt, stream::TryBroadbandExt},
};
use tuwunel_database::{KeyBuf, SEP};

use super::{Check, Report, ScanExt, per_room, tally, undecodable};
use crate::{
	Services,
	rooms::{state_compressor::CompressedState, timeline::Mapped},
};

type Member = (OwnedRoomId, OwnedUserId);

// A room's current state, rebuilt once for its members; none when it has none.
type State = Option<Mapped<Arc<CompressedState>>>;

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

	/// A row leading from the room's current state to the member event is
	/// damaged, the event's timeline mapping included.
	MalformedMapping {
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
/// room's current state, as the membership repair walks them, the state rebuilt
/// once for each run of rows in one room. A key whose first separator is missing
/// or doubled is skipped, as the joined keys check reports it.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	let members = services.db["roomuserid_joined"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|(key, _)| Ok(split(key)));

	per_room(
		members,
		|(room_id, _): &Member| room_id,
		move |room_id| state(services, room_id),
	)
	.broad_and_then(async |member| match member {
		| Ok((member, state)) => verify(services, member, state).await,
		| Err(key) => Ok(Some(Finding::Unparsable { key })),
	})
	.ready_try_filter_map(Ok)
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Contradicted { .. } => "contradicted-join",
			| Self::Unbacked { .. } => "unbacked-join",
			| Self::MalformedMapping { .. } => "malformed-mapping",
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
			| Self::MalformedMapping { room_id, user_id } => {
				write!(f, "a row resolving the member event of {user_id} in {room_id} is damaged")
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

#[tracing::instrument(level = "debug", skip(services))]
async fn state(services: &Services, room_id: OwnedRoomId) -> Result<State> {
	services
		.state_accessor
		.room_state_checked(&room_id)
		.map_ok(|state| state.map(Arc::new))
		.await
		.present()
}

#[tracing::instrument(level = "debug", skip_all)]
async fn verify(
	services: &Services,
	(room_id, user_id): Member,
	state: State,
) -> Result<Option<Finding>> {
	let member = match state {
		| None => return Ok(Some(Finding::Unbacked { room_id, user_id })),
		| Some(Mapped::Damaged) => {
			return Ok(Some(Finding::MalformedMapping { room_id, user_id }));
		},
		| Some(Mapped::Event(state)) =>
			services
				.state_accessor
				.get_member_checked(&state, &user_id)
				.await,
	};

	let finding: Option<Finding> = match member {
		| Ok(Mapped::Event(member)) if member.membership == MembershipState::Join => None,
		| Ok(Mapped::Damaged) => Some(Finding::MalformedMapping { room_id, user_id }),
		| Ok(Mapped::Event(member)) => Finding::Contradicted {
			room_id,
			user_id,
			membership: member.membership,
		}
		.into(),
		| Err(error) if error.is_missing() => Some(Finding::Unbacked { room_id, user_id }),
		| Err(error) if undecodable(&error) => Some(Finding::Unreadable { room_id, user_id }),
		| Err(error) => return Err(error),
	};

	Ok(finding)
}
