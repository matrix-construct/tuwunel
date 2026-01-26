use axum::extract::State;
use futures::{StreamExt, TryStreamExt};
use ruma::api::client::threads::get_threads;
use tuwunel_core::{
	Result, at,
	matrix::{
		Event,
		pdu::{PduCount, PduEvent},
	},
	result::FlatOk,
};

use crate::Ruma;

/// # `GET /_matrix/client/r0/rooms/{roomId}/threads`
pub(crate) async fn get_threads_route(
	State(services): State<crate::State>,
	ref body: Ruma<get_threads::v1::Request>,
) -> Result<get_threads::v1::Response> {
	// Use limit or else 10, with maximum 100
	let limit = body
		.limit
		.map(usize::try_from)
		.flat_ok()
		.unwrap_or(10)
		.min(100);

	let from: PduCount = body
		.from
		.as_deref()
		.map(str::parse)
		.transpose()?
		.unwrap_or_else(PduCount::max);

	let threads: Vec<(PduCount, PduEvent)> = services
		.threads
		.threads_until(body.sender_user(), &body.room_id, from, &body.include)
		.take(limit)
		.try_filter_map(async |(count, pdu)| {
			Ok(services
				.state_accessor
				.user_can_see_event(body.sender_user(), &body.room_id, &pdu.event_id)
				.await
				.then_some((count, pdu)))
		})
		.try_collect()
		.await?;

	Ok(get_threads::v1::Response {
		next_batch: threads
			.last()
			.filter(|_| threads.len() >= limit)
			.map(at!(0))
			.as_ref()
			.map(ToString::to_string),

		chunk: threads
			.into_iter()
			.map(at!(1))
			.map(Event::into_format)
			.collect(),
	})
}
