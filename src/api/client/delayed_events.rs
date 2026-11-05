//! MSC4140 delayed call memberships: the endpoints matrix-js-sdk and
//! matrix-rust-sdk call for a MatrixRTC client's own delayed leave.

use std::time::Duration;

use axum::{
	extract::State,
	http::Uri,
	response::{IntoResponse, Response},
};
use ruma::{
	CanonicalJsonObject,
	api::client::{
		delayed_events::{delayed_state_event, update_delayed_event},
		state::send_state_event,
	},
};
use tuwunel_core::{Err, Result, err};
use tuwunel_matrix::pdu::MAX_PDU_BYTES;
use tuwunel_service::delayed_events::CALL_MEMBER;

use crate::{Ruma, RumaResponse};

/// # `PUT /_matrix/client/*/rooms/{roomId}/state/{eventType}[/{stateKey}]`
///
/// Schedules the state event when the request has the
/// `org.matrix.msc4140.delay` query parameter, in milliseconds, and otherwise
/// sends it.
pub(crate) async fn send_state_event_or_delayed_route(
	State(services): State<crate::State>,
	uri: Uri,
	body: Ruma<send_state_event::v3::Request>,
) -> Result<Response> {
	let Some(delay) = delay(&uri)? else {
		return super::send_state_event_for_key_route(State(services), body)
			.await
			.map(|response| RumaResponse(response).into_response());
	};

	if body.event_type.to_string() != CALL_MEMBER {
		return Err!(Request(Forbidden("Only {CALL_MEMBER} events can be delayed.")));
	}
	let content = body.body.body.json().get();
	if content.len() > MAX_PDU_BYTES {
		return Err!(Request(TooLarge("Delayed events are limited to {MAX_PDU_BYTES} bytes.")));
	}
	let content: CanonicalJsonObject = serde_json::from_str(content)
		.map_err(|error| err!(Request(BadJson("Invalid state event content: {error}"))))?;

	let delay_id = services
		.delayed_events
		.schedule(body.sender_user(), &body.room_id, &body.state_key, content, delay)
		.await?;

	Ok(RumaResponse(delayed_state_event::unstable::Response::new(delay_id)).into_response())
}

fn delay(uri: &Uri) -> Result<Option<Duration>> {
	uri.query()
		.into_iter()
		.flat_map(|query| query.split('&'))
		.find_map(|pair| pair.strip_prefix("org.matrix.msc4140.delay="))
		.map(|millis| {
			millis
				.parse()
				.map(Duration::from_millis)
				.map_err(|_| err!(Request(InvalidParam("Invalid org.matrix.msc4140.delay."))))
		})
		.transpose()
}

/// # `POST /_matrix/client/unstable/org.matrix.msc4140/delayed_events/{delayId}/{action}`
///
/// The form matrix-js-sdk calls first.
pub(crate) async fn update_delayed_event_route(
	State(services): State<crate::State>,
	body: Ruma<update_delayed_event::v1::Request>,
) -> Result<update_delayed_event::v1::Response> {
	services
		.delayed_events
		.update(body.sender_user(), &body.delay_id, &body.action)
		.await?;

	Ok(update_delayed_event::v1::Response::new())
}

/// # `POST /_matrix/client/unstable/org.matrix.msc4140/delayed_events/{delayId}`
///
/// The form with the action in the body, which matrix-rust-sdk calls and
/// matrix-js-sdk falls back to.
pub(crate) async fn update_delayed_event_in_body_route(
	State(services): State<crate::State>,
	body: Ruma<update_delayed_event::unstable_v1::Request>,
) -> Result<update_delayed_event::unstable_v1::Response> {
	services
		.delayed_events
		.update(body.sender_user(), &body.delay_id, &body.action)
		.await?;

	Ok(update_delayed_event::unstable_v1::Response::new())
}
