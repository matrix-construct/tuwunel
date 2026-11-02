use axum::extract::State;
use ruma::{
	api::client::redact::redact_event::v3::{Request, Response},
	events::room::redaction::RoomRedactionEventContent,
};
use tuwunel_core::{Err, Result, result::NotFound, warn};
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::transaction_ids::{RoomScope, RoomTxn};

use crate::{
	Ruma,
	utils::{is_self_redaction, txnid_event_id, txnid_unsigned},
};

/// # `PUT /_matrix/client/v3/rooms/{roomId}/redact/{eventId}/{txnId}`
///
/// Tries to send a redaction event into the room.
///
/// A retried transaction (the same sender and device, room, target and
/// transaction ID) returns the earlier redaction's event ID and sends nothing
/// new.
pub(crate) async fn redact_event_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let sender_user = body.sender_user();
	let sender_device = body.sender_device.as_deref();

	if services.config.rooms.redaction.disable_local
		&& !services.admin.user_is_admin(sender_user).await
	{
		warn!(
			%sender_user,
			event_id = %body.event_id,
			"Local redactions are disabled, non-admin user attempted to redact an event"
		);
		return Err!(Request(Forbidden("Redactions are disabled on this server.")));
	}

	if services.users.is_suspended(sender_user).await
		&& !is_self_redaction(&services, sender_user, &body.event_id).await
	{
		return Err!(Request(UserSuspended("Account is suspended.")));
	}

	let state_lock = services.state.mutex.lock(&body.room_id).await;
	let scope = RoomScope::Redact(&body.event_id);
	let txn = RoomTxn {
		user_id: sender_user,
		device_id: sender_device,
		txn_id: &body.txn_id,
		room_id: &body.room_id,
	};

	let existing = services
		.transaction_ids
		.existing_room_txnid(txn, scope)
		.await
		.optional()?;

	if let Some(response) = existing {
		return txnid_event_id(&response).map(Response::new);
	}

	let redacts = || Some(body.event_id.clone());
	let pdu = PduBuilder {
		redacts: redacts(),
		unsigned: txnid_unsigned(&body.txn_id).into(),
		..PduBuilder::timeline(&RoomRedactionEventContent {
			redacts: redacts(),
			reason: body.reason.clone(),
		})
	};

	let event_id = services
		.timeline
		.build_and_append_pdu(pdu, sender_user, &body.room_id, &state_lock)
		.await?;

	services
		.transaction_ids
		.add_room_txnid(txn, scope, event_id.as_bytes());

	drop(state_lock);

	Ok(Response { event_id })
}
