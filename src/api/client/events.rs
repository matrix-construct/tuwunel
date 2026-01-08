use std::iter::once;

use axum::extract::State;
use futures::StreamExt;
use ruma::api::client::peeking::listen_to_new_events::v3::{Request, Response};
use tokio::time::{Duration, Instant, timeout_at};
use tuwunel_core::{
	Err, Event, Result, at,
	matrix::PduCount,
	utils::{
		BoolExt,
		result::FlatOk,
		stream::{IterStream, ReadyExt},
	},
};

use crate::Ruma;

const EVENT_LIMIT: usize = 50;

/// GET `/_matrix/client/v3/events`
pub(crate) async fn events_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let sender_user = body.sender_user();

	let from = body
		.body
		.from
		.as_deref()
		.map(str::parse)
		.flat_ok()
		.unwrap_or_default();

	let timeout = body
		.body
		.timeout
		.as_ref()
		.map(Duration::as_millis)
		.map(TryInto::try_into)
		.flat_ok()
		.unwrap_or(services.config.client_sync_timeout_default)
		.max(services.config.client_sync_timeout_min)
		.min(services.config.client_sync_timeout_max);

	let Some(room_id) = body.room_id.as_deref() else {
		//TODO: upgrade ruma
		return Err!(Request(InvalidParam("Missing RoomId parameter.")));
	};

	if !services
		.state_accessor
		.user_can_access_room(sender_user, room_id)
		.await
	{
		return Err!(Request(Forbidden("No room preview available.")));
	}

	let stop_at = Instant::now()
		.checked_add(Duration::from_millis(timeout))
		.expect("configuration must limit maximum timeout");

	loop {
		let watchers = services.sync.watch(
			sender_user,
			body.sender_device.as_deref(),
			once(room_id).stream(),
		);

		let next_batch = services.globals.wait_pending().await?;

		let events = services
			.timeline
			.pdus(Some(sender_user), room_id, Some(PduCount::Normal(from)))
			.ready_filter_map(Result::ok)
			.ready_take_while(|(count, _)| PduCount::Normal(next_batch).ge(count))
			.take(EVENT_LIMIT)
			.collect::<Vec<_>>()
			.await;

		if !events.is_empty() {
			return Ok(Response {
				start: events
					.first()
					.map(at!(0))
					.as_ref()
					.map(ToString::to_string),

				end: events
					.last()
					.map(at!(0))
					.as_ref()
					.map(ToString::to_string),

				chunk: events
					.into_iter()
					.map(at!(1))
					.map(Event::into_format)
					.collect(),
			});
		}

		if timeout_at(stop_at, watchers).await.is_err() || services.server.is_stopping() {
			return Ok(Response {
				chunk: Default::default(),
				start: body.body.from,
				end: services
					.server
					.is_stopping()
					.is_false()
					.then_some(next_batch)
					.as_ref()
					.map(ToString::to_string),
			});
		}
	}
}
