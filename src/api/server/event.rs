use axum::extract::State;
use futures::future::try_join;
use ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId, api::federation::event::get_event};
use tuwunel_core::{Result, err};

use super::AccessCheck;
use crate::Ruma;

/// # `GET /_matrix/federation/v1/event/{eventId}`
///
/// Retrieves a single event from the server.
///
/// - Only works if a user of this server is currently invited or joined the
///   room
pub(crate) async fn get_event_route(
	State(services): State<crate::State>,
	body: Ruma<get_event::v1::Request>,
) -> Result<get_event::v1::Response> {
	let event = services
		.timeline
		.get_pdu_json(&body.event_id)
		.await
		.map_err(|_| err!(Request(NotFound("Event not found."))))?;

	let room_id: OwnedRoomId = event
		.get("room_id")
		.and_then(|val| val.as_str())
		.ok_or_else(|| err!(Database("Invalid event in database.")))?
		.try_into()
		.map_err(|_| err!(Database("Invalid room_id in event in database.")))?;

	let access_check = AccessCheck {
		services: &services,
		origin: body.origin(),
		room_id: &room_id,
		event_id: Some(&body.event_id),
	};

	let pdu = async {
		let event = services
			.state_accessor
			.erased_for_server(body.origin(), event)
			.await;

		let pdu = services
			.federation
			.format_pdu_into(event, None)
			.await;

		Ok(pdu)
	};

	let ((), pdu) = try_join(access_check.check(), pdu).await?;

	Ok(get_event::v1::Response {
		origin: services.globals.server_name().to_owned(),
		origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
		pdu,
	})
}
