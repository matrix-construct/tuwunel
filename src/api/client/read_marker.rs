use std::collections::BTreeMap;

use axum::extract::State;
use ruma::{
	MilliSecondsSinceUnixEpoch,
	api::client::{read_marker::set_read_marker, receipt::create_receipt},
	events::{
		RoomAccountDataEventType,
		receipt::{ReceiptThread, ReceiptType},
	},
};
use tuwunel_core::{Err, PduCount, Result, err};

use crate::Ruma;

/// # `POST /_matrix/client/r0/rooms/{roomId}/read_markers`
///
/// Sets different types of read markers.
///
/// - Updates fully-read account data event to `fully_read`
/// - If `read_receipt` is set: Update private marker and public read receipt
///   EDU
pub(crate) async fn set_read_marker_route(
	State(services): State<crate::State>,
	body: Ruma<set_read_marker::v3::Request>,
) -> Result<set_read_marker::v3::Response> {
	let sender_user = body.sender_user();

	if body.private_read_receipt.is_some() || body.read_receipt.is_some() {
		services
			.pusher
			.reset_notification_counts(sender_user, &body.room_id);
	}

	if let Some(event) = &body.fully_read {
		let fully_read_event = ruma::events::fully_read::FullyReadEvent {
			content: ruma::events::fully_read::FullyReadEventContent { event_id: event.clone() },
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
			.private_read_set(&body.room_id, sender_user, count);
	}

	if let Some(event) = &body.read_receipt {
		let receipt_content = BTreeMap::from_iter([(
			event.to_owned(),
			BTreeMap::from_iter([(
				ReceiptType::Read,
				BTreeMap::from_iter([(sender_user.to_owned(), ruma::events::receipt::Receipt {
					ts: Some(MilliSecondsSinceUnixEpoch::now()),
					thread: ReceiptThread::Unthreaded,
				})]),
			)]),
		)]);

		services
			.read_receipt
			.readreceipt_update(
				sender_user,
				&body.room_id,
				&ruma::events::receipt::ReceiptEvent {
					content: ruma::events::receipt::ReceiptEventContent(receipt_content),
					room_id: body.room_id.clone(),
				},
			)
			.await;

		services
			.presence
			.maybe_ping_presence(
				sender_user,
				body.sender_device.as_deref(),
				&ruma::presence::PresenceState::Online,
			)
			.await
			.ok();
	}

	Ok(set_read_marker::v3::Response {})
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/receipt/{receiptType}/{eventId}`
///
/// Sets private read marker and public read receipt EDU.
pub(crate) async fn create_receipt_route(
	State(services): State<crate::State>,
	body: Ruma<create_receipt::v3::Request>,
) -> Result<create_receipt::v3::Response> {
	let sender_user = body.sender_user();

	if matches!(
		&body.receipt_type,
		create_receipt::v3::ReceiptType::Read | create_receipt::v3::ReceiptType::ReadPrivate
	) {
		services
			.pusher
			.reset_notification_counts(sender_user, &body.room_id);
	}

	match body.receipt_type {
		| create_receipt::v3::ReceiptType::FullyRead => {
			let fully_read_event = ruma::events::fully_read::FullyReadEvent {
				content: ruma::events::fully_read::FullyReadEventContent {
					event_id: body.event_id.clone(),
				},
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
					BTreeMap::from_iter([(
						sender_user.to_owned(),
						ruma::events::receipt::Receipt {
							ts: Some(MilliSecondsSinceUnixEpoch::now()),
							thread: ReceiptThread::Unthreaded,
						},
					)]),
				)]),
			)]);

			services
				.read_receipt
				.readreceipt_update(
					sender_user,
					&body.room_id,
					&ruma::events::receipt::ReceiptEvent {
						content: ruma::events::receipt::ReceiptEventContent(receipt_content),
						room_id: body.room_id.clone(),
					},
				)
				.await;

			services
				.presence
				.maybe_ping_presence(
					sender_user,
					body.sender_device.as_deref(),
					&ruma::presence::PresenceState::Online,
				)
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
				.private_read_set(&body.room_id, sender_user, count);
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
