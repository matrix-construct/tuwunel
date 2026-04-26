use axum::extract::State;
use ruma::{
	EventId, RoomId, UserId,
	api::client::room::{report_content, report_room},
	events::room::message,
};
use tuwunel_core::{Err, Result, debug_info, info, matrix::pdu::PduEvent, utils::ReadyExt};
use tuwunel_service::Services;

use crate::{ClientIp, Ruma};

const REASON_MAX_LEN: usize = 750;

/// # `POST /_matrix/client/v3/rooms/{roomId}/report`
///
/// Reports an abusive room to homeserver admins
#[tracing::instrument(skip_all, fields(%client), name = "report_room")]
pub(crate) async fn report_room_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<report_room::v3::Request>,
) -> Result<report_room::v3::Response> {
	let sender_user = body.sender_user();

	info!(
		"Received room report by user {sender_user} for room {} with reason: \"{}\"",
		body.room_id, body.reason,
	);

	if body.reason.len().gt(&REASON_MAX_LEN) {
		return Err!(Request(InvalidParam(
			"Reason too long, should be {REASON_MAX_LEN} characters or fewer"
		)));
	}

	if !services
		.state_cache
		.server_in_room(&services.server.name, &body.room_id)
		.await
	{
		return Err!(Request(NotFound(
			"Room does not exist to us, no local users have joined at all"
		)));
	}

	// send admin room message that we received the report with an @room ping for
	// urgency
	services
		.admin
		.send_message(message::RoomMessageEventContent::text_markdown(format!(
			"@room Room report received from {}\nReport Reason: {}\n\nRoom ID: {}",
			sender_user, body.reason, body.room_id,
		)))
		.await
		.ok();

	Ok(report_room::v3::Response {})
}

/// # `POST /_matrix/client/v3/rooms/{roomId}/report/{eventId}`
///
/// Reports an inappropriate event to homeserver admins
#[tracing::instrument(skip_all, fields(%client), name = "report_event")]
pub(crate) async fn report_event_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<report_content::v3::Request>,
) -> Result<report_content::v3::Response> {
	let sender_user = body.sender_user();
	let reason = body.reason.as_deref().unwrap_or("");

	info!(
		"Received event report by user {sender_user} for room {} and event ID {}, with reason: \
		 \"{}\"",
		body.room_id, body.event_id, reason,
	);

	// check if we know about the reported event ID or if it's invalid
	let Ok(pdu) = services.timeline.get_pdu(&body.event_id).await else {
		return Err!(Request(NotFound("Event ID is not known to us or Event ID is invalid")));
	};

	is_event_report_valid(
		&services,
		&pdu.event_id,
		&body.room_id,
		sender_user,
		body.reason.as_ref(),
		&pdu,
	)
	.await?;

	// send admin room message that we received the report with an @room ping for
	// urgency
	services
		.admin
		.send_message(message::RoomMessageEventContent::text_markdown(format!(
			"@room Event report received from {}\nReport Reason: {}\n\nEvent ID: {}\nRoom ID: \
			 {}\nSent By: {}",
			sender_user, reason, pdu.event_id, pdu.room_id, pdu.sender,
		)))
		.await
		.ok();

	Ok(report_content::v3::Response {})
}

/// in the following order:
///
/// check if the room ID from the URI matches the PDU's room ID
/// check if report reasoning is less than or equal to 750 characters
/// check if reporting user is in the reporting room
async fn is_event_report_valid(
	services: &Services,
	event_id: &EventId,
	room_id: &RoomId,
	sender_user: &UserId,
	reason: Option<&String>,
	pdu: &PduEvent,
) -> Result {
	debug_info!(
		"Checking if report from user {sender_user} for event {event_id} in room {room_id} is \
		 valid"
	);

	if room_id != pdu.room_id {
		return Err!(Request(NotFound("Event ID does not belong to the reported room",)));
	}

	if reason.as_ref().is_some_and(|s| s.len() > 750) {
		return Err!(Request(
			InvalidParam("Reason too long, should be 750 characters or fewer",)
		));
	}

	if !services
		.state_cache
		.room_members(room_id)
		.ready_any(|user_id| user_id == sender_user)
		.await
	{
		return Err!(Request(NotFound("You are not in the room you are reporting.",)));
	}

	Ok(())
}
