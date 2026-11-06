use std::borrow::Cow;

use axum::extract::State;
use futures::{StreamExt, TryFutureExt, pin_mut};
use ruma::{
	EventId, RoomAliasId, RoomId, UserId,
	api::client::room::report_content,
	events::{
		StateEventType,
		room::{create::RoomCreateEventContent, message::RoomMessageEventContent},
	},
	room::RoomType,
};
use serde::Deserialize;
use tuwunel_core::{
	Err, Result, debug_error, debug_info, info,
	utils::{ReadyExt, html::escape, string::single_line},
};
use tuwunel_matrix::{Event, pdu::PduEvent};
use tuwunel_service::Services;

use super::REASON_MAX_LEN;
use crate::{ClientIp, Ruma};

#[cfg(test)]
mod tests;

const DETAIL_MAX_CHARS: usize = 500;

struct Report<'a> {
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
struct Content {
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
		.room_state_get_content(&pdu.room_id, &StateEventType::RoomCreate, "")
		.map_ok(|content: RoomCreateEventContent| content.room_type)
		.inspect_err(|error| debug_error!(%error, "Failed to read reported room type"))
		.await;

	let room_type = room_type
		.as_ref()
		.map(|kind| kind.as_ref().map_or("regular", RoomType::as_str))
		.unwrap_or("unavailable");

	let body = pdu
		.get_content()
		.ok()
		.and_then(|content: Content| content.body);

	let report = {
		let aliases = services
			.alias
			.local_aliases_for_room(&pdu.room_id);

		pin_mut!(aliases);
		let room_alias = aliases.next().await;

		format_report(&Report {
			reporter: sender_user,
			reason,
			event_id: &pdu.event_id,
			room_id: &pdu.room_id,
			room_type,
			room_alias,
			body: body.as_deref(),
			sender: &pdu.sender,
			sender_is_local: services.globals.user_is_local(&pdu.sender),
		})
	};

	services.admin.send_report_content(report).await;

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

fn format_report(report: &Report<'_>) -> RoomMessageEventContent {
	let event_id = single_line(report.event_id.as_str());
	let room_alias = report
		.room_alias
		.map_or("none", RoomAliasId::as_str);

	let (redact_hint, redact_id) = if report.sender_is_local {
		("\n\nTo redact it: !admin users redact-event ", event_id.as_ref())
	} else {
		("", "")
	};

	let body = format!(
		"@room Event report received from {}\nReport Reason: {}\n\nEvent ID: {}\nRoom ID: \
		 {}\nRoom Type: {}\nRoom Alias: {}\nSent By: {}\nMessage: {}{}{}",
		single_line(report.reporter.as_str()),
		single_line(report.reason),
		event_id,
		single_line(report.room_id.as_str()),
		excerpt(report.room_type),
		excerpt(room_alias),
		single_line(report.sender.as_str()),
		excerpt(report.body.unwrap_or("unavailable")),
		redact_hint,
		redact_id,
	);

	let html = escape(&body).replace('\n', "<br>");

	RoomMessageEventContent::text_html(body, html)
}

fn excerpt(value: &str) -> Cow<'_, str> {
	let text = value
		.char_indices()
		.nth(DETAIL_MAX_CHARS)
		.map_or(value, |(end, _)| value.split_at(end).0);

	single_line(text)
}
