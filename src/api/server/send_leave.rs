#![expect(deprecated)]

use axum::extract::State;
use futures::FutureExt;
use ruma::{
	OwnedRoomId, OwnedUserId, RoomId, ServerName,
	api::federation::membership::create_leave_event,
	events::{
		StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{Err, Result, at, err, matrix::event::gen_event_id_canonical_json};
use tuwunel_service::Services;

use crate::Ruma;

/// # `PUT /_matrix/federation/v1/send_leave/{roomId}/{eventId}`
///
/// Submits a signed leave event.
pub(crate) async fn create_leave_event_v1_route(
	State(services): State<crate::State>,
	body: Ruma<create_leave_event::v1::Request>,
) -> Result<create_leave_event::v1::Response> {
	create_leave_event(&services, body.origin(), &body.room_id, &body.pdu).await?;

	Ok(create_leave_event::v1::Response::new())
}

/// # `PUT /_matrix/federation/v2/send_leave/{roomId}/{eventId}`
///
/// Submits a signed leave event.
pub(crate) async fn create_leave_event_v2_route(
	State(services): State<crate::State>,
	body: Ruma<create_leave_event::v2::Request>,
) -> Result<create_leave_event::v2::Response> {
	create_leave_event(&services, body.origin(), &body.room_id, &body.pdu).await?;

	Ok(create_leave_event::v2::Response::new())
}

async fn create_leave_event(
	services: &Services,
	origin: &ServerName,
	room_id: &RoomId,
	pdu: &RawJsonValue,
) -> Result {
	if !services.metadata.exists(room_id).await {
		return Err!(Request(NotFound("Room is unknown to this server.")));
	}

	// ACL check origin
	services
		.event_handler
		.acl_check(origin, room_id)
		.await?;

	// We do not add the event_id field to the pdu here because of signature and
	// hashes checks
	let room_version_id = services.state.get_room_version(room_id).await?;
	let Ok((event_id, value)) = gen_event_id_canonical_json(pdu, &room_version_id) else {
		// Event could not be converted to canonical json
		return Err!(Request(BadJson("Could not convert event to canonical json.")));
	};

	let event_room_id: OwnedRoomId = serde_json::from_value(
		serde_json::to_value(
			value
				.get("room_id")
				.ok_or_else(|| err!(Request(BadJson("Event missing room_id property."))))?,
		)
		.expect("CanonicalJson is valid json value"),
	)
	.map_err(|e| err!(Request(BadJson(warn!("room_id field is not a valid room ID: {e}")))))?;

	if event_room_id != room_id {
		return Err!(Request(BadJson("Event room_id does not match request path room ID.")));
	}

	let content: RoomMemberEventContent = serde_json::from_value(
		value
			.get("content")
			.ok_or_else(|| err!(Request(BadJson("Event missing content property."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson(warn!("Event content is empty or invalid: {e}")))))?;

	if content.membership != MembershipState::Leave {
		return Err!(Request(BadJson(
			"Not allowed to send a non-leave membership event to leave endpoint."
		)));
	}

	let event_type: StateEventType = serde_json::from_value(
		value
			.get("type")
			.ok_or_else(|| err!(Request(BadJson("Event missing type property."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson(warn!("Event has invalid state event type: {e}")))))?;

	if event_type != StateEventType::RoomMember {
		return Err!(Request(BadJson(
			"Not allowed to send non-membership state event to leave endpoint."
		)));
	}

	// ACL check sender server name
	let sender: OwnedUserId = serde_json::from_value(
		value
			.get("sender")
			.ok_or_else(|| err!(Request(BadJson("Event missing sender property."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson(warn!("sender property is not a valid user ID: {e}")))))?;

	services
		.event_handler
		.acl_check(sender.server_name(), room_id)
		.await?;

	if sender.server_name() != origin {
		return Err!(Request(BadJson("Not allowed to leave on behalf of another server/user.")));
	}

	let state_key: OwnedUserId = serde_json::from_value(
		value
			.get("state_key")
			.ok_or_else(|| err!(Request(BadJson("Event missing state_key property."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson(warn!("State key is not a valid user ID: {e}")))))?;

	if state_key != sender {
		return Err!(Request(BadJson("State key does not match sender user.")));
	}

	let mutex_lock = services
		.event_handler
		.mutex_federation
		.lock(room_id)
		.await;

	let pdu_id = services
		.event_handler
		.handle_incoming_pdu(origin, room_id, &event_id, value, true)
		.boxed()
		.await?
		.map(at!(0))
		.ok_or_else(|| err!(Request(InvalidParam("Could not accept as timeline event."))))?;

	drop(mutex_lock);

	services
		.sending
		.send_pdu_room(room_id, &pdu_id)
		.boxed()
		.await
}
