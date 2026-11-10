use std::time::Duration;

use axum::extract::State;
use ruma::{
	MilliSecondsSinceUnixEpoch, UInt,
	api::client::delayed_events::get_delayed_event::v1::{DelayedEventData, Request, Response},
	serde::Raw,
};
use tuwunel_core::Result;
use tuwunel_service::delayed_events::Event;

use crate::Ruma;

/// # `GET /_matrix/client/unstable/org.matrix.msc4140/delayed_events/{delayId}`
///
/// Returns one of the sender's delayed events (MSC4140).
///
/// A finalised event carries its outcome and is kept for about a week; one no
/// longer kept answers 404 like an unknown `delay_id`.
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

	DelayedEventData {
		delay_id,
		room_id: event.room_id,
		event_type: event.event_type,
		state_key: event.state_key,
		delay_ms: Duration::from_millis(event.delay_ms),
		delayed_since_ts: MilliSecondsSinceUnixEpoch(UInt::new_saturating(delayed_since)),
		content: Raw::from_raw_value(event.content.json()),
		finalized: event.finalised,
	}
}
