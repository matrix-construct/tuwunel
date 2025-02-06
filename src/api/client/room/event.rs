use axum::extract::State;
use conduwuit::{err, Err, Event, Result};
use futures::{future::try_join, FutureExt, TryFutureExt};
use ruma::api::client::room::get_room_event;

use crate::{client::is_ignored_pdu, Ruma};

/// # `GET /_matrix/client/r0/rooms/{roomId}/event/{eventId}`
///
/// Gets a single event.
pub(crate) async fn get_room_event_route(
	State(ref services): State<crate::State>,
	ref body: Ruma<get_room_event::v3::Request>,
) -> Result<get_room_event::v3::Response> {
	let event_id = &body.event_id;
	let room_id = &body.room_id;

	let event = services
		.rooms
		.timeline
		.get_pdu(event_id)
		.map_err(|_| err!(Request(NotFound("Event {} not found.", event_id))));

	let visible = services
		.rooms
		.state_accessor
		.user_can_see_event(body.sender_user(), room_id, event_id)
		.map(Ok);

	let (mut event, visible) = try_join(event, visible).await?;

	if !visible || is_ignored_pdu(services, &event, body.sender_user()).await {
		return Err!(Request(Forbidden("You don't have permission to view this event.")));
	}

	debug_assert!(
		event.event_id() == event_id && event.room_id() == room_id,
		"Fetched PDU must match requested"
	);

	event.add_age().ok();

	Ok(get_room_event::v3::Response { event: event.to_room_event() })
}
