use std::collections::BTreeMap;

use axum::extract::State;
use ruma::{
	MilliSecondsSinceUnixEpoch,
	api::client::receipt::create_receipt,
	events::{
		RoomAccountDataEventType,
		fully_read::{FullyReadEvent, FullyReadEventContent},
		receipt::{Receipt, ReceiptEvent, ReceiptEventContent, ReceiptThread, ReceiptType},
	},
};
use tuwunel_core::{Err, PduCount, Result, err};
use tuwunel_service::presence::Ping;

use crate::{ClientIp, Ruma};

/// # `POST /_matrix/client/r0/rooms/{roomId}/receipt/{receiptType}/{eventId}`
///
/// Sets private read marker and public read receipt EDU.
pub(crate) async fn create_receipt_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<create_receipt::v3::Request>,
) -> Result<create_receipt::v3::Response> {
	let sender_user = body.sender_user();

	// MSC3771: thread_id MUST NOT be provided with `m.fully_read`.
	if matches!(&body.receipt_type, create_receipt::v3::ReceiptType::FullyRead)
		&& !matches!(body.thread, ReceiptThread::Unthreaded)
	{
		return Err!(Request(InvalidParam(
			"thread_id must not be set for m.fully_read receipts"
		)));
	}

	// MSC3771: a present thread_id must be a non-empty string.
	if body.thread.as_str() == Some("") {
		return Err!(Request(InvalidParam("thread_id must be a non-empty string")));
	}

	// MSC3771: thread_id is either `"main"` or a thread root event id (which
	// starts with `$`).
	if !matches!(
		&body.thread,
		ReceiptThread::Unthreaded | ReceiptThread::Main | ReceiptThread::Thread(_)
	) {
		return Err!(Request(InvalidParam(
			"thread_id must be either \"main\" or a thread root event id"
		)));
	}

	// MSC3771: event_id must belong to the thread the receipt targets.
	if matches!(&body.thread, ReceiptThread::Main | ReceiptThread::Thread(_)) {
		let resolved = services
			.threads
			.get_thread_id_for_event(&body.event_id)
			.await;

		let in_thread = match (&body.thread, resolved.as_deref()) {
			| (ReceiptThread::Main, None) => true,
			| (ReceiptThread::Thread(root), Some(parent)) => &**root == parent,
			| (ReceiptThread::Thread(root), None) => **root == *body.event_id,
			| _ => false,
		};

		if !in_thread {
			return Err!(Request(InvalidParam("event_id is not related to the given thread_id")));
		}
	}

	if matches!(
		&body.receipt_type,
		create_receipt::v3::ReceiptType::Read | create_receipt::v3::ReceiptType::ReadPrivate
	) {
		services
			.pusher
			.reset_notification_counts_for_thread(sender_user, &body.room_id, &body.thread)
			.await;
	}

	match body.receipt_type {
		| create_receipt::v3::ReceiptType::FullyRead => {
			let fully_read_event = FullyReadEvent {
				content: FullyReadEventContent { event_id: body.event_id.clone() },
			};
			services
				.account_data
				.update(
					Some(&body.room_id),
					sender_user,
					RoomAccountDataEventType::FullyRead,
					&serde_json::to_value(fully_read_event)?,
				)
				.await?;
		},
		| create_receipt::v3::ReceiptType::Read => {
			let receipt_content = BTreeMap::from_iter([(
				body.event_id.clone(),
				BTreeMap::from_iter([(
					ReceiptType::Read,
					BTreeMap::from_iter([(sender_user.to_owned(), Receipt {
						ts: Some(MilliSecondsSinceUnixEpoch::now()),
						thread: body.thread.clone(),
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
		},
		| create_receipt::v3::ReceiptType::ReadPrivate => {
			let count = services
				.timeline
				.get_pdu_count(&body.event_id)
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
					&body.thread,
				)
				.await;
		},
		| _ => {
			return Err!(Request(InvalidParam(warn!(
				"Received unknown read receipt type: {}",
				&body.receipt_type
			))));
		},
	}

	Ok(create_receipt::v3::Response {})
}
