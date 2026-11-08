use std::time::Duration;

use axum::extract::State;
use futures::StreamExt;
use ruma::{
	MilliSecondsSinceUnixEpoch, UInt,
	api::client::delayed_events::{
		DelayedEventData,
		get_all_delayed_events::unstable::{Request, Response},
	},
	serde::Raw,
};
use tuwunel_core::Result;
use tuwunel_service::delayed_events::{DelayId, Event};

use crate::Ruma;

/// # `GET /_matrix/client/unstable/org.matrix.msc4140/delayed_events`
///
/// Lists the sender's scheduled events (MSC4140).
///
/// The response takes the earlier shape, with `delay` and `running_since`,
/// which existing clients read.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_delayed_events_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let delayed_events = services
		.delayed_events
		.scheduled(body.sender_user())
		.map(|(delay_id, event)| event_data(delay_id, event))
		.collect()
		.await;

	Ok(Response::new(delayed_events))
}

fn event_data(delay_id: DelayId, event: Event) -> DelayedEventData {
	let running_since = event.due_ms.saturating_sub(event.delay_ms);

	DelayedEventData::new(
		delay_id.into_string(),
		event.room_id,
		event.event_type,
		event.state_key.as_deref().map(str::to_owned),
		Raw::from_raw_value(event.content.json()),
		Duration::from_millis(event.delay_ms),
		MilliSecondsSinceUnixEpoch(UInt::new_saturating(running_since)),
	)
}
