mod read_markers;
mod receipt;

use futures::future::try_join;
use ruma::{
	EventId, MilliSecondsSinceUnixEpoch, RoomId, UserId,
	events::{
		RoomAccountDataEventType,
		fully_read::{FullyReadEvent, FullyReadEventContent},
		receipt::ReceiptThread,
	},
};
use serde_json::to_value;
use tuwunel_core::{
	Err, Result, debug, debug_warn, err,
	utils::result::{LogErr, NotFound},
};
use tuwunel_matrix::{Event, PduCount, PduId};
use tuwunel_service::{Services, rooms::read_receipt::PrivateRead};

pub(crate) use self::{read_markers::set_read_marker_route, receipt::create_receipt_route};

#[tracing::instrument(level = "trace", skip_all)]
async fn ensure_receipt_visible(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	event_id: &EventId,
) -> Result {
	let event = services
		.timeline
		.get_pdu(event_id)
		.await
		.map_err(|_| err!(Request(NotFound("Event not found."))))?;

	if event.room_id() != room_id
		|| !services
			.state_accessor
			.user_can_see_event(user_id, &event)
			.await
	{
		return Err!(Request(NotFound("Event not found.")));
	}

	Ok(())
}

/// Stores `event` as the user's fully-read marker unless that would move the
/// marker backwards.
///
/// The marker moves only when `event` sits strictly after the stored one or
/// either position does not resolve, so writes that arrive one after another
/// never move it back; two in flight at once are not ordered against each
/// other. MSC4446's `allow_backward` skips the comparison. Positions compare on
/// the global counter, whichever room the event belongs to.
#[tracing::instrument(level = "trace", skip_all)]
async fn set_fully_read(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	event: &EventId,
	allow_backward: bool,
) -> Result {
	if !allow_backward {
		let stored = services
			.account_data
			.get_room(room_id, user_id, RoomAccountDataEventType::FullyRead)
			.await
			.optional()
			.inspect_err(
				|error| debug_warn!(%user_id, %room_id, %error, "Unreadable fully-read marker, replacing it"),
			)
			.ok()
			.flatten()
			.map(|stored: FullyReadEvent| stored.content.event_id);

		if !services
			.read_receipt
			.receipt_advanced(stored.as_deref(), event)
			.await
		{
			return Ok(());
		}
	}

	let fully_read = FullyReadEvent {
		content: FullyReadEventContent { event_id: event.to_owned() },
	};

	services
		.account_data
		.update(
			Some(room_id),
			user_id,
			RoomAccountDataEventType::FullyRead,
			&to_value(fully_read)?,
		)
		.await
}

/// Resolves `event` to its timeline position and stores the private read
/// marker for `thread` there.
///
/// Returns whether the marker advanced. A backfilled event carries no forward
/// position, so it is skipped like a non-advancing write rather than failing
/// the request.
async fn set_private_marker(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	event: &EventId,
	thread: &ReceiptThread,
) -> Result<bool> {
	let (pdu_id, shortroomid) =
		try_join(services.timeline.get_pdu_id(event), services.short.get_shortroomid(room_id))
			.await
			.map_err(|_| err!(Request(NotFound("Event not found."))))?;

	let pdu_id = PduId::from(pdu_id);

	if pdu_id.shortroomid != shortroomid {
		return Err!(Request(NotFound("Event not found.")));
	}

	let PduCount::Normal(count) = pdu_id.count else {
		debug!(%user_id, %room_id, %event, "Skipping private read marker at a backfilled event");
		return Ok(false);
	};

	let advanced = services
		.read_receipt
		.private_read_set(PrivateRead {
			room_id,
			user_id,
			count,
			ts: MilliSecondsSinceUnixEpoch::now(),
			thread,
			announce: true,
		})
		.await;

	Ok(advanced)
}

/// Clears the receipt's notification counts and refreshes the push badge.
///
/// The refresh follows every advance because the gateway can hold a stale
/// badge while the stored count is already zero; only a delivery reconciles
/// it.
async fn reset_and_refresh_badge(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	acknowledged: Option<&EventId>,
	thread: &ReceiptThread,
) {
	services
		.pusher
		.reset_notification_counts_for_thread(user_id, room_id, acknowledged, thread)
		.await;

	services
		.sending
		.refresh_push_badge(user_id)
		.await
		.log_err()
		.ok();
}
