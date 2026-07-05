mod mindroom_edits;
mod v3;
mod v5;

use futures::{FutureExt, StreamExt, pin_mut};
use ruma::{RoomId, UserId, events::TimelineEventType::RoomMember};
use tuwunel_core::{
	Error, PduCount, Result,
	matrix::{Event, pdu::PduEvent},
	utils::{
		result::LogErr,
		stream::{BroadbandExt, ReadyExt},
	},
};
use tuwunel_service::Services;

use self::mindroom_edits::collapse_superseded_edits;
pub(crate) use self::{
	v3::{calculate_heroes, sync_events_route},
	v5::sync_events_v5_route,
};

async fn load_timeline(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	roomsincecount: PduCount,
	next_batch: Option<PduCount>,
	limit: usize,
) -> Result<(Vec<(PduCount, PduEvent)>, bool, PduCount), Error> {
	let last_timeline_count = services
		.timeline
		.last_timeline_count(Some(sender_user), room_id, next_batch)
		.await?;

	if last_timeline_count <= roomsincecount {
		return Ok((Vec::new(), false, last_timeline_count));
	}

	let non_timeline_pdus = services
		.timeline
		.pdus_rev(Some(sender_user), room_id, None)
		.ready_filter_map(Result::ok)
		.ready_skip_while(|&(pducount, _)| pducount > next_batch.unwrap_or_else(PduCount::max))
		.ready_take_while(|&(pducount, _)| pducount > roomsincecount);

	// Take the last events for the timeline
	pin_mut!(non_timeline_pdus);
	let timeline_pdus: Vec<_> = non_timeline_pdus
		.by_ref()
		.take(limit)
		.collect()
		.map(|mut pdus: Vec<_>| {
			pdus.reverse();
			pdus
		})
		.await;

	// They /sync response doesn't always return all messages, so we say the output
	// is limited unless there are events in non_timeline_pdus
	let limited = non_timeline_pdus.next().await.is_some();

	// Collapse superseded m.replace events when enabled
	let timeline_pdus = if services
		.server
		.config
		.mindroom_compact_edits_enabled
	{
		collapse_superseded_edits(timeline_pdus)
	} else {
		timeline_pdus
	};

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
