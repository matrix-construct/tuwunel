use std::time::Duration;

use axum::extract::State;
use ruma::{
	MilliSecondsSinceUnixEpoch, UInt,
	api::client::delayed_events::get_delayed_event::v1::{DelayedEventData, Request, Response},
	serde::Raw,
};
use tuwunel_core::Result;
use tuwunel_matrix::StateKey;
use tuwunel_service::delayed_events::Event;

use crate::Ruma;

/// # `GET /_matrix/client/unstable/org.matrix.msc4140/delayed_events/{delayId}`
///
/// Returns one of the sender's scheduled events (MSC4140).
///
/// An event that was sent or cancelled is gone, so it answers 404 like an
/// unknown `delay_id`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_delayed_event_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let event = services
		.delayed_events
		.get(&body.delay_id, Some(body.sender_user()))
		.await?;

	Ok(Response::new(event_data(body.body.delay_id, event)))
}

fn event_data(delay_id: String, event: Event) -> DelayedEventData {
	let delayed_since = event.due_ms.saturating_sub(event.delay_ms);
	let data = DelayedEventData::new(
		delay_id,
		event.room_id,
		event.event_type,
		Raw::from_raw_value(event.content.json()),
		Duration::from_millis(event.delay_ms),
		MilliSecondsSinceUnixEpoch(UInt::new_saturating(delayed_since)),
	);

	with_state_key(data, event.state_key)
}

// non_exhaustive: no struct literal
fn with_state_key(mut data: DelayedEventData, state_key: Option<StateKey>) -> DelayedEventData {
	data.state_key = state_key;
	data
}
