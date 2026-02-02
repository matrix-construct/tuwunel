use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use ruma::{
	EventId, RoomId, UserId,
	api::client::room::{report_content, report_room},
	events::room::message,
	int,
};
use tuwunel_core::{Err, Result, debug_info, info, matrix::pdu::PduEvent, utils::ReadyExt};
use tuwunel_service::Services;

use crate::Ruma;

const REASON_MAX_LEN: usize = 750;

/// # `POST /_matrix/client/v3/rooms/{roomId}/report`
///
/// Reports an abusive room to homeserver admins
#[tracing::instrument(skip_all, fields(%client), name = "report_room")]
pub(crate) async fn report_room_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<report_room::v3::Request>,
) -> Result<report_room::v3::Response> {
	// user authentication
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
			"@room Room report received from {} -\n\nRoom ID: {}\n\nReport Reason: {}",
			sender_user.to_owned(),
			body.room_id,
			body.reason,
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
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<report_content::v3::Request>,
) -> Result<report_content::v3::Response> {
	// user authentication
	let sender_user = body.sender_user();

	info!(
		"Received event report by user {sender_user} for room {} and event ID {}, with reason: \
		 \"{}\"",
		body.room_id,
		body.event_id,
		body.reason.as_deref().unwrap_or("")
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
		body.score,
		&pdu,
	)
	.await?;

	// send admin room message that we received the report with an @room ping for
	// urgency
	services
		.admin
		.send_message(message::RoomMessageEventContent::text_markdown(format!(
			"@room Event report received from {} -\n\nEvent ID: {}\nRoom ID: {}\nSent By: \
			 {}\n\nReport Score: {}\nReport Reason: {}",
			sender_user.to_owned(),
			pdu.event_id,
			pdu.room_id,
			pdu.sender,
			body.score.unwrap_or_else(|| ruma::Int::from(0)),
			body.reason.as_deref().unwrap_or("")
		)))
		.await
		.ok();

	Ok(report_content::v3::Response {})
}

/// in the following order:
///
/// check if the room ID from the URI matches the PDU's room ID
/// check if score is in valid range
/// check if report reasoning is less than or equal to 750 characters
/// check if reporting user is in the reporting room
async fn is_event_report_valid(
	services: &Services,
	event_id: &EventId,
	room_id: &RoomId,
	sender_user: &UserId,
	reason: Option<&String>,
	score: Option<ruma::Int>,
	pdu: &PduEvent,
) -> Result {
	debug_info!(
		"Checking if report from user {sender_user} for event {event_id} in room {room_id} is \
		 valid"
	);

	if room_id != pdu.room_id {
		return Err!(Request(NotFound("Event ID does not belong to the reported room",)));
	}

	if score.is_some_and(|s| s > int!(0) || s < int!(-100)) {
		return Err!(Request(InvalidParam("Invalid score, must be within 0 to -100",)));
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
