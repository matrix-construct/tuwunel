mod v3;
mod v5;

use futures::{StreamExt, pin_mut};
use ruma::{RoomId, UserId, events::TimelineEventType::RoomMember};
use tuwunel_core::{
	Error, PduCount, Result,
	matrix::{Event, pdu::PduEvent},
	utils::{ReadyExt, result::LogErr, stream::BroadbandExt},
};
use tuwunel_service::Services;

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
