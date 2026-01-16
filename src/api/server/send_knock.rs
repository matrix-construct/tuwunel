use axum::extract::State;
use futures::{FutureExt, future::try_join};
use ruma::{
	OwnedServerName, OwnedUserId,
	RoomVersionId::*,
	api::federation::membership::create_knock_event,
	events::{
		StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	serde::JsonObject,
};
use tuwunel_core::{
	Err, Result, at, err,
	matrix::{event::gen_event_id_canonical_json, pdu::PduEvent},
	warn,
};

use crate::Ruma;

/// # `PUT /_matrix/federation/v1/send_knock/{roomId}/{eventId}`
///
/// Submits a signed knock event.
pub(crate) async fn create_knock_event_v1_route(
	State(services): State<crate::State>,
	body: Ruma<create_knock_event::v1::Request>,
) -> Result<create_knock_event::v1::Response> {
	if let Some(server) = body.room_id.server_name()
		&& services
			.config
			.forbidden_remote_server_names
			.is_match(server.host())
	{
		warn!(
			"Server {} tried knocking room ID {} which has a server name that is globally \
			 forbidden. Rejecting.",
			body.origin(),
			&body.room_id,
		);
		return Err!(Request(Forbidden("Server is banned on this homeserver.")));
	}

	if !services.metadata.exists(&body.room_id).await {
		return Err!(Request(NotFound("Room is unknown to this server.")));
	}

	// ACL check origin server
	services
		.event_handler
		.acl_check(body.origin(), &body.room_id)
		.await?;

	let room_version_id = services
		.state
		.get_room_version(&body.room_id)
		.await?;

	if matches!(room_version_id, V1 | V2 | V3 | V4 | V5 | V6) {
		return Err!(Request(Forbidden("Room version does not support knocking.")));
	}

	let Ok((event_id, value)) = gen_event_id_canonical_json(&body.pdu, &room_version_id) else {
		// Event could not be converted to canonical json
		return Err!(Request(InvalidParam("Could not convert event to canonical json.")));
	};

	let event_type: StateEventType = serde_json::from_value(
		value
			.get("type")
			.ok_or_else(|| err!(Request(InvalidParam("Event has no event type."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(InvalidParam("Event has invalid event type: {e}"))))?;

	if event_type != StateEventType::RoomMember {
		return Err!(Request(InvalidParam(
			"Not allowed to send non-membership state event to knock endpoint.",
		)));
	}

	let content: RoomMemberEventContent = serde_json::from_value(
		value
			.get("content")
			.ok_or_else(|| err!(Request(InvalidParam("Membership event has no content"))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(InvalidParam("Event has invalid membership content: {e}"))))?;

	if content.membership != MembershipState::Knock {
		return Err!(Request(InvalidParam(
			"Not allowed to send a non-knock membership event to knock endpoint."
		)));
	}

	// ACL check sender server name
	let sender: OwnedUserId = serde_json::from_value(
		value
			.get("sender")
			.ok_or_else(|| err!(Request(InvalidParam("Event has no sender user ID."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson("Event sender is not a valid user ID: {e}"))))?;

	services
		.event_handler
		.acl_check(sender.server_name(), &body.room_id)
		.await?;

	// check if origin server is trying to send for another server
	if sender.server_name() != body.origin() {
		return Err!(Request(BadJson("Not allowed to knock on behalf of another server/user.")));
	}

	let state_key: OwnedUserId = serde_json::from_value(
		value
			.get("state_key")
			.ok_or_else(|| err!(Request(InvalidParam("Event does not have a state_key"))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson("Event does not have a valid state_key: {e}"))))?;

	if state_key != sender {
		return Err!(Request(InvalidParam("state_key does not match sender user of event.")));
	}

	let origin: OwnedServerName = serde_json::from_value(
		value
			.get("origin")
			.ok_or_else(|| err!(Request(BadJson("Event does not have an origin server name."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson("Event has an invalid origin server name: {e}"))))?;

	let mut event: JsonObject = serde_json::from_str(body.pdu.get())
		.map_err(|e| err!(Request(InvalidParam("Invalid knock event PDU: {e}"))))?;

	event.insert("event_id".to_owned(), "$placeholder".into());

	let pdu: PduEvent = serde_json::from_value(event.into())
		.map_err(|e| err!(Request(InvalidParam("Invalid knock event PDU: {e}"))))?;

	let mutex_lock = services
		.event_handler
		.mutex_federation
		.lock(&body.room_id)
		.await;

	let pdu_id = services
		.event_handler
		.handle_incoming_pdu(&origin, &body.room_id, &event_id, value.clone(), true)
		.boxed()
		.await?
		.map(at!(0))
		.ok_or_else(|| err!(Request(InvalidParam("Could not accept as timeline event."))))?;

	drop(mutex_lock);

	let broadcast = services
		.sending
		.send_pdu_room(&body.room_id, &pdu_id);

	let knock_room_state = services.state.summary_stripped(&pdu).map(Ok);

	let (knock_room_state, ()) = try_join(knock_room_state, broadcast).await?;

	Ok(create_knock_event::v1::Response {
		knock_room_state: knock_room_state
			.into_iter()
			.map(Into::into)
			.collect(),
	})
}
