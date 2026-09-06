mod read_markers;
mod receipt;

use ruma::{
	EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, UserId,
	events::{
		RoomAccountDataEventType,
		fully_read::{FullyReadEvent, FullyReadEventContent},
		receipt::ReceiptThread,
	},
};
use tuwunel_core::{Err, PduCount, Result, err, utils::result::LogErr};
use tuwunel_service::{Services, rooms::read_receipt::PrivateRead};

pub(crate) use self::{read_markers::set_read_marker_route, receipt::create_receipt_route};

/// Stores the fully-read marker unless it would move the position backwards:
/// a stale device must not regress the marker another one already advanced.
/// An unresolvable position on either side accepts the write, as for public
/// receipts.
async fn set_fully_read(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	event: &EventId,
) -> Result<()> {
	let current: Option<OwnedEventId> = services
		.account_data
		.get_room::<serde_json::Value>(room_id, user_id, RoomAccountDataEventType::FullyRead)
		.await
		.ok()
		.and_then(|value| {
			let event_id = value.pointer("/content/event_id")?.as_str()?;

			EventId::parse(event_id).ok()
		});

	let advances = match current {
		| Some(current) => {
			let current = services
				.timeline
				.get_pdu_count(&current)
				.await
				.ok();
			let incoming = services.timeline.get_pdu_count(event).await.ok();

			current
				.zip(incoming)
				.is_none_or(|(current, incoming)| incoming > current)
		},
		| None => true,
	};

	if !advances {
		return Ok(());
	}

	services
		.account_data
		.update(
			Some(room_id),
			user_id,
			RoomAccountDataEventType::FullyRead,
			&serde_json::to_value(FullyReadEvent {
				content: FullyReadEventContent { event_id: event.to_owned() },
			})?,
		)
		.await
}

/// Resolves `event` to its timeline position and stores the private read
/// marker for `thread` there.
///
/// Returns whether the marker advanced. A backfilled event carries no forward
/// position, so it is rejected rather than stored.
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

	let PduCount::Normal(count) = count else {
		return Err!(Request(InvalidParam(
			"Event is a backfilled PDU and cannot be marked as read."
		)));
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
