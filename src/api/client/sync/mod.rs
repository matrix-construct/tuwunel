#[cfg(test)]
mod tests;
mod v3;
mod v5;

use futures::{StreamExt, pin_mut};
use ruma::{
	OwnedUserId, RoomId, UserId,
	events::{
		AnyStrippedStateEvent, TimelineEventType::RoomMember,
		invite_permission_config::InvitePermission,
	},
	serde::Raw,
};
use tuwunel_core::{
	Error, PduCount, Result, is_equal_to,
	matrix::{Event, pdu::PduEvent},
	utils::{ReadyExt, result::LogErr, stream::BroadbandExt},
};
use tuwunel_service::{Services, users::InviteFilter};

pub(crate) use self::{
	v3::{calculate_heroes, sync_events_route},
	v5::sync_events_v5_route,
};

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

async fn share_encrypted_room(
	services: &Services,
	sender_user: &UserId,
	user_id: &UserId,
	ignore_room: Option<&RoomId>,
) -> bool {
	services
		.state_cache
		.get_shared_rooms(sender_user, user_id)
		.ready_filter(|&room_id| Some(room_id) != ignore_room)
		.map(ToOwned::to_owned)
		.broad_any(async |other_room_id| {
			services
				.state_accessor
				.is_encrypted_room(&other_room_id)
				.await
		})
		.await
}

/// MSC4155: whether a stored invite may be served to the invitee.
///
/// The verdict is the recipient's own, judged against the sender of the
/// stripped invite membership event. An invite whose sender cannot be derived
/// is withheld from a filtering user rather than shown to them.
async fn invite_permitted_room(
	services: &Services,
	user_id: &UserId,
	filter: &InviteFilter,
	room_id: &RoomId,
) -> bool {
	filter.is_permissive()
		|| services
			.state_cache
			.invite_state(user_id, room_id)
			.await
			.is_ok_and(|invite_state| invite_permitted(user_id, filter, &invite_state))
}

/// [`invite_permitted_room`] for a room whose stripped state is in hand.
///
/// Callers walking stored invites already hold the state and take this form,
/// which spares them the load the room-keyed form pays per room.
fn invite_permitted(
	user_id: &UserId,
	filter: &InviteFilter,
	invite_state: &[Raw<AnyStrippedStateEvent>],
) -> bool {
	filter.is_permissive()
		|| invite_sender(user_id, invite_state).is_some_and(|sender| {
			filter
				.permission(&sender)
				.eq(&InvitePermission::Allow)
		})
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

/// State sections strip the stored `prev_content`/`prev_sender` pair
/// (Synapse injects the pair on timeline fetches only). The requester's own
/// membership and events duplicated from the returned timeline (MSC4222,
/// full_state) keep it: clients read membership transitions from those
/// copies.
fn strip_prev_state(
	mut pdu: PduEvent,
	sender_user: &UserId,
	in_timeline: impl Fn(&PduEvent) -> bool,
) -> PduEvent {
	let own_membership =
		*pdu.kind() == RoomMember && pdu.state_key() == Some(sender_user.as_str());

	if !own_membership && !in_timeline(&pdu) {
		pdu.remove_prev_state().log_err().ok();
	}

	pdu
}
