mod profiles;
#[cfg(test)]
mod tests;
mod v3;
mod v5;

use futures::{StreamExt, pin_mut};
use ruma::{
	EventId, OwnedUserId, RoomId, UserId,
	events::{
		AnyStrippedStateEvent, AnySyncStateEvent, StateEventType,
		TimelineEventType::{RoomCreate, RoomMember},
		room::member::MembershipState,
	},
	serde::Raw,
	uint,
};
use tuwunel_core::{
	Error, Result, debug_warn, is_equal_to,
	utils::{
		result::{LogErr, NotFound},
		time::now_millis,
	},
	warn,
};
use tuwunel_matrix::{
	Event, PduCount,
	pdu::{EventHash, PduEvent},
};
use tuwunel_service::{Services, users::InviteFilter};

pub use self::v3::calculate_heroes;
pub(crate) use self::{v3::sync_events_route, v5::sync_events_v5_route};

#[derive(Clone, Copy)]
enum TimelineErrors {
	Ignore,
	Propagate,
}

async fn load_timeline(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	roomsincecount: PduCount,
	next_batch: Option<PduCount>,
	limit: usize,
) -> Result<(Vec<(PduCount, PduEvent)>, bool, PduCount), Error> {
	load_timeline_with_errors(
		services,
		sender_user,
		room_id,
		roomsincecount,
		next_batch,
		limit,
		TimelineErrors::Ignore,
	)
	.await
}

async fn load_timeline_fallible(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	roomsincecount: PduCount,
	next_batch: Option<PduCount>,
	limit: usize,
) -> Result<(Vec<(PduCount, PduEvent)>, bool, PduCount), Error> {
	load_timeline_with_errors(
		services,
		sender_user,
		room_id,
		roomsincecount,
		next_batch,
		limit,
		TimelineErrors::Propagate,
	)
	.await
}

async fn load_timeline_with_errors(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	roomsincecount: PduCount,
	next_batch: Option<PduCount>,
	limit: usize,
	errors: TimelineErrors,
) -> Result<(Vec<(PduCount, PduEvent)>, bool, PduCount), Error> {
	let until = next_batch.map(|count| count.saturating_add(1));
	let pdus = services
		.timeline
		.pdus_rev(Some(sender_user), room_id, until);

	// Take the last events for the timeline.
	pin_mut!(pdus);
	let mut timeline_pdus = Vec::new();
	let mut last_timeline_count = PduCount::max();
	let mut first = true;
	let mut limited = false;

	while let Some(pdu) = pdus.next().await {
		let (pducount, pdu) = match pdu {
			| Ok(pdu) => pdu,
			| Err(error) if first || matches!(errors, TimelineErrors::Propagate) => {
				return Err(error);
			},
			| Err(_) => continue,
		};

		if first {
			first = false;
			last_timeline_count = matches!(pducount, PduCount::Normal(_))
				.then_some(pducount)
				.unwrap_or_else(PduCount::max);
		}

		if pducount <= roomsincecount {
			break;
		}

		if timeline_pdus.len() == limit {
			limited = true;
			break;
		}

		timeline_pdus.push((pducount, pdu));
	}

	timeline_pdus.reverse();

	Ok((timeline_pdus, limited, last_timeline_count))
}

/// Returns the backward pagination token for a timeline slice.
///
/// A slice beginning at room creation has nothing before it, so its window's
/// end lets a members query describe the room as received.
/// Backward pagination from that token re-reads the slice once before ending.
fn timeline_prev_batch(
	timeline_pdus: &[(PduCount, PduEvent)],
	window_end: PduCount,
) -> Option<PduCount> {
	timeline_pdus
		.first()
		.map(|(count, pdu)| match pdu.kind() {
			| RoomCreate => window_end,
			| _ => *count,
		})
}

async fn share_encrypted_room(
	services: &Services,
	sender_user: &UserId,
	user_id: &UserId,
) -> bool {
	services
		.state_cache
		.get_shared_rooms(sender_user, user_id)
		.map(ToOwned::to_owned)
		.any(async |room_id| {
			services
				.state_accessor
				.is_encrypted_room(&room_id)
				.await
		})
		.await
}

/// Whether a user's join to a room is fresh since a sync token.
///
/// The joined count moves on every join and forced state replay except a local
/// user's profile change over the position its join recorded, which keeps it.
/// An unreadable count reads as unmoved.
#[tracing::instrument(level = "trace", skip(services))]
async fn joined_since(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	since: u64,
) -> bool {
	services
		.state_cache
		.get_joined_count(room_id, user_id)
		.await
		.inspect_err(|error| debug_warn!(%room_id, %user_id, %error, "joined count unreadable"))
		.is_ok_and(|recorded| recorded > since)
}

/// MSC4155: whether a stored invite may be served to the invitee.
///
/// The verdict is the recipient's own, judged against the sender of the
/// stripped invite membership event. Unreadable invite state takes the same
/// verdict as the sender-less case below, since neither can name a sender.
async fn invite_permitted_room(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	filter: &InviteFilter,
) -> bool {
	filter.is_permissive()
		|| services
			.state_cache
			.invite_state(user_id, room_id)
			.await
			.map_or_else(
				|error| {
					debug_warn!(%user_id, %room_id, ?error, "invite state is unreadable; skipping the sender rules");
					filter.permits(None)
				},
				|invite_state| invite_permitted(user_id, room_id, filter, &invite_state),
			)
}

/// [`invite_permitted_room`] for a room whose stripped state is in hand.
///
/// Callers walking stored invites already hold the state and take this form,
/// which spares them the load the room-keyed form pays per room. `room_id`
/// names the affected row in the sender-less diagnostic and does not enter the
/// verdict. That diagnostic stays at debug on a release build deliberately,
/// since the condition repeats for every sync an affected user makes.
fn invite_permitted(
	user_id: &UserId,
	room_id: &RoomId,
	filter: &InviteFilter,
	invite_state: &[Raw<AnyStrippedStateEvent>],
) -> bool {
	// Load-bearing: keeps a permissive user off the sender derivation below.
	if filter.is_permissive() {
		return true;
	}

	let sender = invite_sender(user_id, invite_state);

	if sender.is_none() {
		debug_warn!(%user_id, %room_id, "invite state names no sender; skipping the sender rules");
	}

	filter.permits(sender.as_deref())
}

/// The sender of the stripped membership event inviting `user_id`.
///
/// The last matching entry wins. An invite this server recorded after the
/// federation route began sanitising stripped state holds one entry for this
/// cell, our own copy of the signed membership PDU, whose sender the origin
/// check authenticated. An invite stored before that still carries whatever
/// the inviting server sent ahead of our copy, and the array has no ordering
/// semantics in the spec, so reading the last entry is what keeps those
/// answering with the authenticated sender too.
fn invite_sender(
	user_id: &UserId,
	invite_state: &[Raw<AnyStrippedStateEvent>],
) -> Option<OwnedUserId> {
	invite_state
		.iter()
		.rev()
		.filter(|event| {
			event
				.get_field::<&str>("state_key")
				.is_ok_and(|state_key| state_key.is_some_and(is_equal_to!(user_id.as_str())))
		})
		.filter_map(|event| event.deserialize().ok())
		.find_map(|event| match event {
			| AnyStrippedStateEvent::RoomMember(member) if member.state_key == user_id =>
				Some(member.sender),
			| _ => None,
		})
}

/// Strips the unsigned fields a state section does not serve.
///
/// Fields private to the sender go unless the requester sent the event. The
/// stored `prev_content`/`prev_sender` pair goes too (Synapse injects it on
/// timeline fetches only), except on the requester's own membership and on
/// events duplicated from the returned timeline (MSC4222, full_state), the
/// copies clients read membership transitions from.
fn strip_state_unsigned(
	mut pdu: PduEvent,
	sender_user: &UserId,
	in_timeline: impl Fn(&PduEvent) -> bool,
) -> PduEvent {
	let own_membership =
		*pdu.kind() == RoomMember && pdu.state_key() == Some(sender_user.as_str());

	pdu.remove_private_unless_sender(Some(sender_user));

	if !own_membership && !in_timeline(&pdu) {
		pdu.remove_prev_state().log_err().ok();
	}

	pdu
}

/// The user's own departure in a left room's state, when it holds one.
///
/// A ban counts, since the membership indexes file a banned user as left. Any
/// other membership yields `None`, as a room this server left still holds the
/// invite a later rescission withdrew. A failed read is logged and yields `None`
/// too, so the client still learns of the departure. A kick or ban is sent by
/// someone else, so fields private to the sender are removed unless the user
/// sent it.
async fn own_departure(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
) -> Option<PduEvent> {
	services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())
		.await
		.optional()
		.inspect_err(|error| warn!(%room_id, %user_id, %error, "Unreadable own departure event"))
		.ok()
		.flatten()
		.filter(|event| {
			matches!(
				event.membership_for(user_id),
				Some(MembershipState::Leave | MembershipState::Ban)
			)
		})
		.map(|event| event.without_private_unless_sender(Some(user_id)))
}

/// Builds a minimal leave event for a left room whose state supplies none.
///
/// A rescinded or rejected federated invite, or deleted, missing, or broken
/// room state, leaves no real membership event to send, while clients read the
/// departure from one. The stand-in names the user as sender under a fresh
/// local event ID.
fn stand_in_leave(
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
) -> Result<Raw<AnySyncStateEvent>> {
	let event = PduEvent {
		event_id: EventId::new_v1(services.globals.server_name()),
		origin_server_ts: now_millis().try_into()?,
		kind: RoomMember,
		state_key: Some(sender_user.as_str().into()),
		sender: sender_user.to_owned(),
		content: serde_json::from_str(r#"{"membership":"leave"}"#)?,
		// The following keys are dropped on conversion
		room_id: room_id.to_owned(),
		depth: uint!(1),
		origin: None,
		unsigned: None,
		redacts: None,
		hashes: EventHash::default(),
		auth_events: Default::default(),
		prev_events: Default::default(),
	};

	Ok(event.into_format())
}
