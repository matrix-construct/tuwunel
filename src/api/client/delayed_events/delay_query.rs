use std::time::Duration;

use axum::{
	extract::{FromRequestParts, State},
	response::{IntoResponse, Response},
};
use futures::FutureExt;
use http::request::Parts;
use ruma::api::client::{
	delayed_events::{
		delayed_message_event::unstable::Response as DelayedMessageResponse,
		delayed_state_event::unstable::Response as DelayedStateResponse,
	},
	message::send_message_event,
	state::send_state_event,
};
use tuwunel_core::{Error, Result, err};
use url::form_urlencoded::parse as parse_query;

use super::{Kind, Schedule, schedule};
use crate::{
	Ruma, RumaResponse, send::send_message_event_route, state::send_state_event_for_key_route,
};

pub(super) const DELAY_PARAM: &str = "org.matrix.msc4140.delay";

/// The `org.matrix.msc4140.delay` query parameter, in milliseconds.
///
/// Absent when the request does not carry it. Present with no value or a value
/// that is not a whole number, it refuses the request rather than sending the
/// event at once.
pub(crate) struct DelayQuery(Option<u64>);

/// # `PUT /_matrix/client/v3/rooms/{roomId}/send/{eventType}/{txnId}`
///
/// Sends a message event, or schedules it to be sent later (MSC4140).
///
/// A request carrying `org.matrix.msc4140.delay` schedules the event and
/// answers with its `delay_id`; any other goes to the ordinary send route.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn send_message_event_or_delayed_route(
	State(services): State<crate::State>,
	DelayQuery(delay): DelayQuery,
	body: Ruma<send_message_event::v3::Request>,
) -> Result<Response> {
	let Some(delay) = delay else {
		return send_message_event_route(State(services), body)
			.boxed() // size firewall
			.await
			.map(|response| RumaResponse(response).into_response());
	};

	let kind = Kind::Message {
		event_type: &body.event_type,
		content: &body.body.body,
	};

	let request = Schedule {
		sender: body.sender_user(),
		device: body.sender_device.as_deref(),
		room_id: &body.room_id,
		kind,
		delay: Duration::from_millis(delay),
		txn_id: Some(&body.txn_id),
		timestamp: body.appservice_info.as_ref().and(body.timestamp),
		stable: false,
	};

	let delay_id = schedule(&services, request)
		.boxed() // size firewall
		.await?;

	let response = DelayedMessageResponse::new(delay_id.into_string());

	Ok(RumaResponse(response).into_response())
}

/// # `PUT /_matrix/client/v3/rooms/{roomId}/state/{eventType}/{stateKey}`
///
/// Sends a state event, or schedules it to be sent later (MSC4140).
///
/// A request carrying `org.matrix.msc4140.delay` schedules the event and
/// answers with its `delay_id`; any other goes to the ordinary state route.
/// It also serves the empty-key paths.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn send_state_event_or_delayed_route(
	State(services): State<crate::State>,
	DelayQuery(delay): DelayQuery,
	body: Ruma<send_state_event::v3::Request>,
) -> Result<Response> {
	let Some(delay) = delay else {
		return send_state_event_for_key_route(State(services), body)
			.boxed() // size firewall
			.await
			.map(|response| RumaResponse(response).into_response());
	};

	let kind = Kind::State {
		event_type: &body.event_type,
		state_key: &body.state_key,
		content: &body.body.body,
	};

	let request = Schedule {
		sender: body.sender_user(),
		device: body.sender_device.as_deref(),
		room_id: &body.room_id,
		kind,
		delay: Duration::from_millis(delay),
		txn_id: None,
		timestamp: body.appservice_info.as_ref().and(body.timestamp),
		stable: false,
	};

	let delay_id = schedule(&services, request)
		.boxed() // size firewall
		.await?;

	let response = DelayedStateResponse::new(delay_id.into_string());

	Ok(RumaResponse(response).into_response())
}

impl FromRequestParts<crate::State> for DelayQuery {
	type Rejection = Error;

	async fn from_request_parts(
		parts: &mut Parts,
		_services: &crate::State,
	) -> Result<Self, Self::Rejection> {
		parse_delay(parts.uri.query().unwrap_or_default()).map(Self)
	}
}

/// Reads the delay parameter from a raw query string.
///
/// When the parameter repeats, the first occurrence wins.
pub(super) fn parse_delay(query: &str) -> Result<Option<u64>> {
	parse_query(query.as_bytes())
		.find(|(key, _)| key == DELAY_PARAM)
		.map(|(_, value)| {
			value
				.parse()
				.map_err(|e| err!(Request(InvalidParam("Invalid {DELAY_PARAM}: {e}"))))
		})
		.transpose()
}
