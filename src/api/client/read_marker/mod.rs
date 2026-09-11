mod read_markers;
mod receipt;

use futures::future::try_join;
use ruma::{EventId, MilliSecondsSinceUnixEpoch, RoomId, UserId, events::receipt::ReceiptThread};
use tuwunel_core::{Err, PduCount, PduId, Result, debug, err, utils::result::LogErr};
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
