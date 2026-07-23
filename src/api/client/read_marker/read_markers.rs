use std::collections::BTreeMap;

use axum::extract::State;
use ruma::{
	MilliSecondsSinceUnixEpoch,
	api::client::read_marker::set_read_marker,
	events::{
		RoomAccountDataEventType,
		fully_read::{FullyReadEvent, FullyReadEventContent},
		receipt::{Receipt, ReceiptEvent, ReceiptEventContent, ReceiptThread, ReceiptType},
	},
};
use tuwunel_core::{Err, PduCount, Result, err};
use tuwunel_service::presence::Ping;

use crate::{ClientIp, Ruma};

/// # `POST /_matrix/client/r0/rooms/{roomId}/read_markers`
///
/// Sets different types of read markers.
///
/// - Updates fully-read account data event to `fully_read`
/// - If `read_receipt` is set: Update private marker and public read receipt
///   EDU
pub(crate) async fn set_read_marker_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<set_read_marker::v3::Request>,
) -> Result<set_read_marker::v3::Response> {
	let sender_user = body.sender_user();

	if body.private_read_receipt.is_some() || body.read_receipt.is_some() {
		// Route through the dispatcher so per-thread counts are also cleared;
		// `/read_markers` predates MSC3771 and carries no thread field.
		services
			.pusher
			.reset_notification_counts_for_thread(
				sender_user,
				&body.room_id,
				&ReceiptThread::Unthreaded,
			)
			.await;
	}

	if let Some(event) = &body.fully_read {
		let fully_read_event = FullyReadEvent {
			content: FullyReadEventContent { event_id: event.clone() },
		};

		services
			.account_data
			.update(
				Some(&body.room_id),
				sender_user,
				RoomAccountDataEventType::FullyRead,
				&serde_json::to_value(fully_read_event)?,
			)
			.await
			.ok();
	}

	if let Some(event) = &body.private_read_receipt {
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

		services
			.read_receipt
			.private_read_set(
				&body.room_id,
				sender_user,
				count,
				MilliSecondsSinceUnixEpoch::now(),
				&ReceiptThread::Unthreaded,
			)
			.await;
	}

	if let Some(event) = &body.read_receipt {
		let receipt_content = BTreeMap::from_iter([(
			event.to_owned(),
			BTreeMap::from_iter([(
				ReceiptType::Read,
				BTreeMap::from_iter([(sender_user.to_owned(), Receipt {
					ts: Some(MilliSecondsSinceUnixEpoch::now()),
					thread: ReceiptThread::Unthreaded,
				})]),
			)]),
		)]);

		services
			.read_receipt
			.readreceipt_update(sender_user, &body.room_id, &ReceiptEvent {
				content: ReceiptEventContent(receipt_content),
				room_id: body.room_id.clone(),
			})
			.await;

		let ping = Ping {
			device_id: body.sender_device.as_deref(),
			client_ip: Some(client),
			appservice: body.appservice_info.as_ref(),
			..Default::default()
		};

		services
			.presence
			.maybe_ping_presence(sender_user, ping)
			.await
			.ok();
	}

	Ok(set_read_marker::v3::Response {})
}
