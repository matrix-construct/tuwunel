use axum::extract::State;
use ruma::api::federation::event::get_event_by_timestamp::v1;
use tuwunel_core::{Err, Result};

use crate::router::Ruma;

/// # `GET /_matrix/federation/v1/timestamp_to_event/{roomId}`
///
/// Get the ID of the event closest to the given timestamp.
pub(crate) async fn get_event_by_timestamp_route(
	State(services): State<crate::State>,
	body: Ruma<v1::Request>,
) -> Result<v1::Response> {
	let origin = body.origin();
	let room_id = &body.room_id;

	// check if the server is allowed to see the room
	services
		.event_handler
		.acl_check(origin, room_id)
		.await?;

	// get the closest event to the timestamp
	let (origin_server_ts, event_id) = services
		.timeline
		.get_event_id_near_ts(room_id, body.ts, body.dir)
		.await?;

	// check if the server is allowed to see the event
	if !services
		.state_accessor
		.server_can_see_event(origin, room_id, &event_id)
		.await
	{
		return Err!(Request(Forbidden("Server is not allowed to see this event")));
	}

	Ok(v1::Response::new(event_id, origin_server_ts))
}
