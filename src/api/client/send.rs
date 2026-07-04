use std::collections::BTreeMap;

use axum::extract::State;
use futures::{FutureExt, future::try_join4};
use ruma::{
	DeviceId, RoomId, TransactionId, UserId,
	api::client::message::send_message_event,
	events::{
		AnyMessageLikeEventContent, MessageLikeEventType,
		reaction::ReactionEventContent,
		room::{encrypted::Relation, redaction::RoomRedactionEventContent},
	},
	serde::Raw,
};
use serde::Deserialize;
use serde_json::from_str;
use tuwunel_core::{
	Err, Result, debug_warn, err,
	matrix::{Event, pdu::PduBuilder},
	utils::{self},
	warn,
};
use tuwunel_service::Services;

use crate::{Ruma, client::utils::is_self_redaction};

#[derive(Deserialize)]
struct ExtractRelatesTo {
	#[serde(rename = "m.relates_to")]
	relates_to: Relation,
}

/// # `PUT /_matrix/client/v3/rooms/{roomId}/send/{eventType}/{txnId}`
///
/// Send a message event into the room.
///
/// - Is a NOOP if the txn id was already used before and returns the same event
///   id again
/// - The only requirement for the content is that it has to be valid json
/// - Tries to send the event into the room, auth rules will determine if it is
///   allowed
pub(crate) async fn send_message_event_route(
	State(services): State<crate::State>,
	body: Ruma<send_message_event::v3::Request>,
) -> Result<send_message_event::v3::Response> {
	let sender_user = body.sender_user();
	let sender_device = body.sender_device.as_deref();
	let appservice_info = body.appservice_info.as_ref();

	// Forbid m.room.encrypted if encryption is disabled
	if body.event_type == MessageLikeEventType::RoomEncrypted && !services.config.allow_encryption
	{
		return Err!(Request(Forbidden("Encryption has been disabled")));
	}

	// MSC4169: clients sending m.room.redaction via /send put `redacts` in
	// `content`. Pre-v11 auth rules read it from the top level; lift it so
	// `redacts_id(...)` resolves regardless of room version. Mirrors the
	// /redact handler.
	let redaction_content = || {
		body.body
			.body
			.deserialize_as_unchecked::<RoomRedactionEventContent>()
			.inspect_err(|_| {
				debug_warn!(
					%sender_user,
					event = %body.body.body.json(),
					"Client sent invalid redaction event"
				);
			})
			.ok()
	};

	let redacts_id = body
		.event_type
		.eq(&MessageLikeEventType::RoomRedaction)
		.then(redaction_content)
		.flatten()
		.and_then(|content| content.redacts);

	if body.event_type == MessageLikeEventType::RoomRedaction
		&& services.config.disable_local_redactions
		&& !services.admin.user_is_admin(sender_user).await
	{
		warn!(
			%sender_user,
			?redacts_id,
			"Local redactions are disabled, non-admin user attempted to redact an event"
		);

		return Err!(Request(Forbidden("Redactions are disabled on this server.")));
	}

	if services.users.is_suspended(sender_user).await {
		if body.event_type != MessageLikeEventType::RoomRedaction {
			return Err!(Request(UserSuspended(
				"Cannot send non-redaction events while suspended."
			)));
		}

		let is_self = match &redacts_id {
			| None => false,
			| Some(redacts_id) => is_self_redaction(&services, sender_user, redacts_id).await,
		};

		if !is_self {
			return Err!(Request(UserSuspended("Can only redact own events while suspended.")));
		}
	}

	let state_lock = services.state.mutex.lock(&body.room_id).await;

	let (existing_txnid, ..) = try_join4(
		check_existing_txnid(&services, sender_user, sender_device, &body.txn_id).map(Ok),
		check_duplicate_reaction(&services, &body.event_type, sender_user, &body.body.body),
		check_public_call_invite(&services, &body.event_type, &body.room_id),
		check_nested_thread(&services, &body.body.body),
	)
	.await?;

	if let Some(existing_txnid) = existing_txnid {
		return existing_txnid;
	}

	let mut unsigned = BTreeMap::new();
	unsigned.insert("transaction_id".to_owned(), body.txn_id.to_string().into());

	let content = from_str(body.body.body.json().get())
		.map_err(|e| err!(Request(BadJson("Invalid JSON body: {e}"))))?;

	let event_id = services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: body.event_type.clone().into(),
				content,
				unsigned: Some(unsigned),
				timestamp: appservice_info.and(body.timestamp),
				redacts: redacts_id,
				..Default::default()
			},
			sender_user,
			&body.room_id,
			&state_lock,
		)
		.await?;

	services.transaction_ids.add_txnid(
		sender_user,
		sender_device,
		&body.txn_id,
		event_id.as_bytes(),
	);

	drop(state_lock);

	Ok(send_message_event::v3::Response { event_id })
}

async fn check_public_call_invite(
	services: &Services,
	event_type: &MessageLikeEventType,
	room_id: &RoomId,
) -> Result {
	if *event_type != MessageLikeEventType::CallInvite {
		return Ok(());
	}

	if !services.directory.is_public_room(room_id).await {
		return Ok(());
	}

	Err!(Request(Forbidden("Room call invites are not allowed in public rooms")))
}

// Forbid duplicate reactions
async fn check_duplicate_reaction(
	services: &Services,
	event_type: &MessageLikeEventType,
	sender_user: &UserId,
	body: &Raw<AnyMessageLikeEventContent>,
) -> Result {
	if *event_type != MessageLikeEventType::Reaction {
		return Ok(());
	}

	let Ok(content) = body.deserialize_as_unchecked::<ReactionEventContent>() else {
		return Ok(());
	};

	if !services
		.pdu_metadata
		.event_has_relation(
			&content.relates_to.event_id,
			Some(sender_user),
			None,
			Some(&content.relates_to.key),
		)
		.await
	{
		return Ok(());
	}

	Err!(Request(DuplicateAnnotation("Duplicate reactions are not allowed.")))
}

// MSC3440/Matrix 1.4: a thread may only target an event which itself carries
// no rel_type; the spec assigns this rejection 400 M_UNKNOWN.
async fn check_nested_thread(
	services: &Services,
	body: &Raw<AnyMessageLikeEventContent>,
) -> Result {
	let Ok(ExtractRelatesTo { relates_to: Relation::Thread(thread) }) =
		body.deserialize_as_unchecked()
	else {
		return Ok(());
	};

	let Ok(root) = services.timeline.get_pdu(&thread.event_id).await else {
		return Ok(());
	};

	let nested = root
		.get_content()
		.is_ok_and(|content: ExtractRelatesTo| content.relates_to.rel_type().is_some());

	if !nested {
		return Ok(());
	}

	Err!(Request(Unknown("Cannot start threads from an event with a relation.")))
}

/// Check if this is a new transaction id. Returns Some when the transaction id
/// exists and the send must then be terminated by returning the contained
/// result.
async fn check_existing_txnid(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	txn_id: &TransactionId,
) -> Option<Result<send_message_event::v3::Response>> {
	let Ok(response) = services
		.transaction_ids
		.existing_txnid(sender_user, sender_device, txn_id)
		.await
	else {
		return None;
	};

	// The client might have sent a txnid of the /sendToDevice endpoint
	// This txnid has no response associated with it
	if response.is_empty() {
		return Some(Err!(Request(InvalidParam(
			"Tried to use txn_id already used for an incompatible endpoint."
		))));
	}

	let Ok(Ok(event_id)) = utils::string_from_bytes(&response).map(TryInto::try_into) else {
		return Some(Err!(Database("Invalid event_id in txn_id data: {response:?}.")));
	};

	Some(Ok(send_message_event::v3::Response { event_id }))
}
