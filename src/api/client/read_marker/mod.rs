mod read_markers;
mod receipt;

use ruma::{EventId, MilliSecondsSinceUnixEpoch, RoomId, UserId, events::receipt::ReceiptThread};
use tuwunel_core::{PduCount, Result, err, utils::result::LogErr};
use tuwunel_service::{Services, rooms::read_receipt::PrivateRead};

pub(crate) use self::{read_markers::set_read_marker_route, receipt::create_receipt_route};

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
	let count = services
		.timeline
		.get_pdu_count(event)
		.await
		.map_err(|_| err!(Request(NotFound("Event not found."))))?;

	// A backfilled event has no forward position; its position is below every
	// live event, so storing it can never advance the marker.
	let PduCount::Normal(count) = count else {
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
	thread: &ReceiptThread,
) {
	services
		.pusher
		.reset_notification_counts_for_thread(user_id, room_id, thread)
		.await;

	services
		.sending
		.refresh_push_badge(user_id)
		.await
		.log_err()
		.ok();
}
