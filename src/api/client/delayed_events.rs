//! Client-server endpoints for MSC4140 delayed events.

use axum::{
	extract::{OriginalUri, State},
	response::{IntoResponse, Response},
};
use ruma::{
	CanonicalJsonObject,
	api::client::{
		delayed_events::{
			delayed_message_event, delayed_state_event, get_delayed_event, send_delayed_event,
			update_delayed_event,
		},
		message::send_message_event,
		state::send_state_event,
	},
};
use tuwunel_core::{Err, Result, err};
use tuwunel_service::delayed_events::{Action, ScheduleParams};

use crate::{ClientIp, Ruma, RumaResponse};

fn parse_content(json: &str) -> Result<CanonicalJsonObject> {
	serde_json::from_str(json)
		.map_err(|error| err!(Request(BadJson("Invalid delayed event content: {error}"))))
}

fn is_stable(uri: &http::Uri) -> bool {
	!uri.path()
		.starts_with("/_matrix/client/unstable/")
}

fn delay_from_query(uri: &http::Uri) -> Result<Option<std::time::Duration>> {
	let Some(query) = uri.query() else {
		return Ok(None);
	};

	query
		.split('&')
		.filter_map(|part| part.split_once('='))
		.find_map(|(key, value)| (key == "org.matrix.msc4140.delay").then_some(value))
		.map(|value| {
			value
				.parse::<u64>()
				.map(std::time::Duration::from_millis)
				.map_err(|_| err!(Request(InvalidParam("Invalid org.matrix.msc4140.delay."))))
		})
		.transpose()
}

/// Dispatches the ordinary message-send path and the deprecated MSC4140
/// query-parameter form, which intentionally share the same URL path.
pub(crate) async fn send_message_event_or_delayed_route(
	State(services): State<crate::State>,
	OriginalUri(uri): OriginalUri,
	body: Ruma<send_message_event::v3::Request>,
) -> Result<Response> {
	if let Some(delay) = delay_from_query(&uri)? {
		let delay_id = services
			.delayed_events
			.schedule(ScheduleParams {
				user_id: body.sender_user(),
				device_id: body.sender_device.as_deref(),
				room_id: body.room_id.clone(),
				event_type: body.event_type.clone().into(),
				state_key: None,
				content: parse_content(body.body.body.json().get())?,
				txn_id: Some(body.txn_id.clone()),
				delay,
				timestamp: body.appservice_info.as_ref().and(body.timestamp),
				stable: false,
			})
			.await?;

		return Ok(RumaResponse(delayed_message_event::unstable::Response::new(delay_id))
			.into_response());
	}

	Ok(RumaResponse(super::send_message_event_route(State(services), body).await?)
		.into_response())
}

/// Dispatches the ordinary state-send path and the deprecated MSC4140
/// query-parameter form.
pub(crate) async fn send_state_event_or_delayed_route(
	State(services): State<crate::State>,
	OriginalUri(uri): OriginalUri,
	body: Ruma<send_state_event::v3::Request>,
) -> Result<Response> {
	if let Some(delay) = delay_from_query(&uri)? {
		let delay_id = services
			.delayed_events
			.schedule(ScheduleParams {
				user_id: body.sender_user(),
				device_id: body.sender_device.as_deref(),
				room_id: body.room_id.clone(),
				event_type: body.event_type.clone().into(),
				state_key: Some(body.state_key.clone()),
				content: parse_content(body.body.body.json().get())?,
				txn_id: None,
				delay,
				timestamp: body.appservice_info.as_ref().and(body.timestamp),
				stable: false,
			})
			.await?;

		return Ok(
			RumaResponse(delayed_state_event::unstable::Response::new(delay_id)).into_response()
		);
	}

	Ok(
		RumaResponse(super::send_state_event_for_key_route(State(services), body).await?)
			.into_response(),
	)
}

/// `PUT /_matrix/client/v3/rooms/{room_id}/delayed_event/{event_type}/{txn_id}`
pub(crate) async fn send_delayed_event_route(
	State(services): State<crate::State>,
	OriginalUri(uri): OriginalUri,
	body: Ruma<send_delayed_event::v3::Request>,
) -> Result<send_delayed_event::v3::Response> {
	let delay_id = services
		.delayed_events
		.schedule(ScheduleParams {
			user_id: body.sender_user(),
			device_id: body.sender_device.as_deref(),
			room_id: body.room_id.clone(),
			event_type: body.event_type.clone(),
			state_key: body
				.state_key
				.as_ref()
				.map(|state_key| state_key.as_str().to_owned()),
			content: parse_content(body.content.json().get())?,
			txn_id: Some(body.txn_id.clone()),
			delay: body.delay_ms,
			timestamp: body.appservice_info.as_ref().and(body.ts),
			stable: is_stable(&uri),
		})
		.await?;

	Ok(send_delayed_event::v3::Response::new(delay_id))
}

/// `POST /_matrix/client/v1/delayed_events/{delay_id}/{action}`
///
/// Delegated LiveKit JWT services call the unstable path without an access
/// token, as MSC4140 allowed before it was accepted, so only that path serves
/// requests without one.
pub(crate) async fn update_delayed_event_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	OriginalUri(uri): OriginalUri,
	body: Ruma<update_delayed_event::v1::Request>,
) -> Result<update_delayed_event::v1::Response> {
	let owner = body.sender_user.as_deref();
	if owner.is_none() && is_stable(&uri) {
		return Err!(Request(MissingToken("Missing access token.")));
	}

	services
		.delayed_events
		.update(&body.delay_id, Action::try_from(&body.action)?, owner, client)
		.await?;

	Ok(update_delayed_event::v1::Response::new())
}

/// `POST /_matrix/client/unstable/org.matrix.msc4140/delayed_events/{delay_id}`
///
/// The earlier form with the action in the body, still used by Element X.
pub(crate) async fn update_delayed_event_legacy_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<update_delayed_event::unstable_v1::Request>,
) -> Result<update_delayed_event::unstable_v1::Response> {
	services
		.delayed_events
		.update(
			&body.delay_id,
			Action::try_from(&body.action)?,
			Some(body.sender_user()),
			client,
		)
		.await?;

	Ok(update_delayed_event::unstable_v1::Response::new())
}

/// `GET /_matrix/client/v1/delayed_events/{delay_id}`
pub(crate) async fn get_delayed_event_route(
	State(services): State<crate::State>,
	body: Ruma<get_delayed_event::v1::Request>,
) -> Result<get_delayed_event::v1::Response> {
	Ok(get_delayed_event::v1::Response::new(
		services
			.delayed_events
			.get(&body.delay_id, body.sender_user())
			.await?,
	))
}

#[cfg(test)]
mod tests {
	use super::delay_from_query;

	#[test]
	fn parses_legacy_delay_query() {
		let uri = "/_matrix/client/v3/rooms/!room:example.org/send/m.room.message/tx?foo=bar&\
		           org.matrix.msc4140.delay=123";
		assert_eq!(
			delay_from_query(&uri.parse().unwrap())
				.unwrap()
				.unwrap()
				.as_millis(),
			123
		);
	}

	#[test]
	fn ignores_requests_without_a_delay_query() {
		assert!(
			delay_from_query(&"/path".parse().unwrap())
				.unwrap()
				.is_none()
		);
	}
}
