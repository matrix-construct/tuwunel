use axum::extract::State;
use ruma::api::client::room::get_event_by_timestamp::v1;
use tuwunel_core::{Err, Result};

use crate::router::Ruma;

/// # `GET /_matrix/client/v1/rooms/{roomId}/timestamp_to_event`
///
/// Get the ID of the event closest to the given timestamp.
pub(crate) async fn get_event_by_timestamp_route(
	State(services): State<crate::State>,
	body: Ruma<v1::Request>,
) -> Result<v1::Response> {
	let sender_user = body.sender_user();
	let room_id = &body.room_id;

	// check if user can see the room
	if !services
		.state_accessor
		.user_can_see_state_events(sender_user, room_id)
		.await
	{
		return Err!(Request(Forbidden("You don't have permission to view this room.")));
	}

	// get the closest event to the given timestamp
	let (origin_server_ts, event_id) = services
		.timeline
		.get_event_id_near_ts(room_id, body.ts, body.dir)
		.await?;

	if !services
		.state_accessor
		.user_can_see_event(sender_user, room_id, &event_id)
		.await
	{
		return Err!(Request(Forbidden("You don't have permission to view this event.")));
	}

	// return the closest event found locally or from federation
	Ok(v1::Response::new(event_id, origin_server_ts))
}
