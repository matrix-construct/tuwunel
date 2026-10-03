use axum::extract::State;
use ruma::{EventId, RoomId, UserId, api::client::room::report_content};
use tuwunel_core::{Err, Result, debug_info, info, utils::ReadyExt};
use tuwunel_matrix::pdu::PduEvent;
use tuwunel_service::Services;

use super::REASON_MAX_LEN;
use crate::{ClientIp, Ruma};

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

	services
		.admin
		.send_report(&format!(
			"@room Event report received from {}\nReport Reason: {}\n\nEvent ID: {}\nRoom ID: \
			 {}\nSent By: {}",
			sender_user, reason, pdu.event_id, pdu.room_id, pdu.sender,
		))
		.await;

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

	if reason
		.as_ref()
		.is_some_and(|s| s.len() > REASON_MAX_LEN)
	{
		return Err!(Request(InvalidParam(
			"Reason too long, should be {REASON_MAX_LEN} characters or fewer"
		)));
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
