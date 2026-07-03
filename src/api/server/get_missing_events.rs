use axum::extract::State;
use ruma::{CanonicalJsonValue, EventId, api::federation::event::get_missing_events};
use tuwunel_core::{Result, debug};

use super::AccessCheck;
use crate::Ruma;

/// arbitrary number but synapse's is 20 and we can handle lots of these anyways
const LIMIT_MAX: usize = 50;
/// spec says default is 10
const LIMIT_DEFAULT: usize = 10;

/// # `POST /_matrix/federation/v1/get_missing_events/{roomId}`
///
/// Retrieves events that the sender is missing.
pub(crate) async fn get_missing_events_route(
	State(services): State<crate::State>,
	body: Ruma<get_missing_events::v1::Request>,
) -> Result<get_missing_events::v1::Response> {
	AccessCheck {
		services: &services,
		origin: body.origin(),
		room_id: &body.room_id,
		event_id: None,
	}
	.check()
	.await?;

	let limit = body
		.limit
		.try_into()
		.unwrap_or(LIMIT_DEFAULT)
		.min(LIMIT_MAX);

	let room_version = services
		.state
		.get_room_version(&body.room_id)
		.await
		.ok();

	let mut queued_events = body.latest_events.clone();
	// the vec will never have more entries the limit
	let mut events = Vec::with_capacity(limit);

	let mut i: usize = 0;
	while i < queued_events.len() && events.len() < limit {
		let Ok(event) = services
			.timeline
			.get_pdu_json(&queued_events[i])
			.await
		else {
			debug!(
				?body.origin,
				event_id = %queued_events[i],
				"Event does not exist locally, skipping"
			);
			i = i.saturating_add(1);
			continue;
		};

		if body.earliest_events.contains(&queued_events[i]) {
			i = i.saturating_add(1);
			continue;
		}

		if !services
			.state_accessor
			.server_can_see_event(body.origin(), &body.room_id, &queued_events[i])
			.await
		{
			debug!(
				?body.origin,
				event_id = %queued_events[i],
				room_id = %body.room_id,
				"Server cannot see event, skipping"
			);
			i = i.saturating_add(1);
			continue;
		}

		let prev_events = event
			.get("prev_events")
			.and_then(CanonicalJsonValue::as_array)
			.into_iter()
			.flatten()
			.filter_map(CanonicalJsonValue::as_str)
			.filter_map(|id| EventId::parse(id).ok());

		queued_events.extend(prev_events);

		let event = services
			.state_accessor
			.erased_for_server(body.origin(), event)
			.await;

		let event = services
			.federation
			.format_pdu_into(event, room_version.as_ref())
			.await;

		events.push(event);
	}

	Ok(get_missing_events::v1::Response { events })
}
