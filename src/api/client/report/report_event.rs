use axum::extract::State;
use futures::{StreamExt, pin_mut};
use ruma::{EventId, RoomAliasId, RoomId, UserId, api::client::room::report_content};
use serde::Deserialize;
use tuwunel_core::{
	Err, Result, debug_info, info,
	utils::{ReadyExt, TryFutureExtExt, html, string::markdown_cell},
};
use tuwunel_matrix::{Event, pdu::PduEvent};
use tuwunel_service::Services;

use super::REASON_MAX_LEN;
use crate::{ClientIp, Ruma};

#[cfg(test)]
mod tests;

/// Reported message text longer than this many characters is cut off.
const BODY_MAX_CHARS: usize = 500;

struct EventReport<'a> {
	reporter: &'a UserId,
	reason: &'a str,
	event_id: &'a EventId,
	room_id: &'a RoomId,
	room_type: &'a str,
	room_alias: Option<&'a RoomAliasId>,
	body: Option<&'a str>,
	sender: &'a UserId,
	sender_is_local: bool,
}

#[derive(Deserialize)]
struct ReportedContent {
	body: Option<String>,
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

	let room_type = services
		.state_accessor
		.get_room_type(&pdu.room_id)
		.ok()
		.await;

	let room_type = room_type.map_or_else(|| "regular".to_owned(), |kind| kind.to_string());

	let aliases = services
		.alias
		.local_aliases_for_room(&pdu.room_id);

	pin_mut!(aliases);
	let room_alias = aliases.next().await;

	let body = pdu
		.get_content::<ReportedContent>()
		.ok()
		.and_then(|content| content.body);

	let report = EventReport {
		reporter: sender_user,
		reason,
		event_id: &pdu.event_id,
		room_id: &pdu.room_id,
		room_type: &room_type,
		room_alias,
		body: body.as_deref(),
		sender: &pdu.sender,
		sender_is_local: services.globals.user_is_local(&pdu.sender),
	};

	services
		.admin
		.send_report(&format_report(&report))
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

fn format_report(report: &EventReport<'_>) -> String {
	let room_alias = report
		.room_alias
		.map_or("none", RoomAliasId::as_str);

	let body = report
		.body
		.map_or_else(|| "unavailable".to_owned(), quote_body);

	let redact_hint = if report.sender_is_local {
		format!("\n\nTo redact it: `!admin users redact-event {}`", report.event_id)
	} else {
		String::new()
	};

	format!(
		"@room Event report received from {}\nReport Reason: {}\n\nEvent ID: {}\nRoom ID: \
		 {}\nRoom Type: {}\nRoom Alias: {}\nSent By: {}\nMessage: {}{}",
		report.reporter,
		report.reason,
		report.event_id,
		report.room_id,
		report.room_type,
		room_alias,
		report.sender,
		body,
		redact_hint,
	)
}

/// Makes text written by the reported user safe to show in a markdown message.
fn quote_body(body: &str) -> String {
	let text: String = body.chars().take(BODY_MAX_CHARS).collect();

	html::escape(&markdown_cell(&text)).replace('[', "\\[")
}
