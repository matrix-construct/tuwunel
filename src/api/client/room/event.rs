use axum::extract::State;
use futures::{TryFutureExt, future::join3, pin_mut};
use ruma::api::client::room::get_room_event;
use tuwunel_core::{
	Err, Event, Pdu, Result, err,
	result::IsErrOr,
	utils::{BoolExt, FutureBoolExt, TryFutureExtExt, future::OptionFutureExt},
};

use crate::{Ruma, client::is_ignored_pdu};

/// # `GET /_matrix/client/r0/rooms/{roomId}/event/{eventId}`
///
/// Gets a single event.
pub(crate) async fn get_room_event_route(
	State(services): State<crate::State>,
	body: Ruma<get_room_event::v3::Request>,
) -> Result<get_room_event::v3::Response> {
	let sender_user = body.sender_user();
	let event_id = &body.event_id;
	let room_id = &body.room_id;

	let event = services
		.timeline
		.get_pdu(event_id)
		.map_err(|_| err!(Request(NotFound("Event {} not found.", event_id))));

	let retained_event = body
		.include_unredacted_content
		.then_async(async || {
			let is_admin = services.admin.user_is_admin(sender_user);

			let can_redact = services
				.config
				.allow_room_admins_to_request_unredacted_events
				.then_async(|| {
					services
						.state_accessor
						.get_power_levels(room_id)
						.map_ok_or(false, |power_levels| {
							power_levels.for_user(sender_user) >= power_levels.redact
						})
				})
				.unwrap_or(false);

			pin_mut!(is_admin, can_redact);

			if is_admin.or(can_redact).await {
				services
					.retention
					.get_original_pdu(event_id)
					.await
					.map_err(|_| err!(Request(NotFound("Event {} not found.", event_id))))
			} else {
				Err!(Request(Forbidden("You are not allowed to see the original event")))
			}
		});

	let visible = services
		.state_accessor
		.user_can_see_event(sender_user, room_id, event_id);

	let (mut event, retained_event, visible): (Result<Pdu>, Option<Result<Pdu>>, _) =
		join3(event, retained_event, visible).await;

	if event.as_ref().is_err_or(Event::is_redacted)
		&& let Some(retained_event) = retained_event
	{
		event = retained_event;
	}

	let mut event = event?;

	if !visible || is_ignored_pdu(&services, &event, body.sender_user()).await {
		return Err!(Request(Forbidden("You don't have permission to view this event.")));
	}

	debug_assert!(
		event.event_id() == event_id && event.room_id() == room_id,
		"Fetched PDU must match requested"
	);

	event.add_age().ok();

	Ok(get_room_event::v3::Response { event: event.into_format() })
}
