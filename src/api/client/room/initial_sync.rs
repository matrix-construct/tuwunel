use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::try_join5};
use ruma::{
	api::client::room::initial_sync::v3::{PaginationChunk, Request, Response},
	events::AnyRawAccountDataEvent,
};
use tuwunel_core::{
	Err, Event, Result, at, extract_variant,
	matrix::PduCount,
	utils::stream::{ReadyExt, TryTools},
};

use crate::Ruma;

const LIMIT_MAX: usize = 50;

/// GET `/_matrix/client/v3/rooms/{roomId}/initialSync`
pub(crate) async fn room_initial_sync_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let room_id = &body.room_id;

	if !services
		.state_accessor
		.user_can_access_room(body.sender_user(), room_id)
		.await
	{
		return Err!(Request(Forbidden("No room preview available.")));
	}

	let next_batch = services.globals.current_count();

	let visibility = services.directory.visibility(room_id).map(Ok);

	let membership = services
		.state_cache
		.user_membership(body.sender_user(), room_id)
		.map(Ok);

	let state = services
		.state_accessor
		.room_state_full_pdus(room_id)
		.map_ok(Event::into_format)
		.try_collect::<Vec<_>>();

	let limit = LIMIT_MAX;
	let events = services
		.timeline
		.pdus_rev(None, room_id, Some(PduCount::Normal(next_batch).saturating_add(1)))
		.try_take(limit)
		.try_collect()
		.map_ok(|mut vec: Vec<_>| {
			vec.reverse();
			vec
		});

	let account_data = services
		.account_data
		.changes_since(Some(room_id), body.sender_user(), 0, Some(next_batch))
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Room))
		.collect::<Vec<_>>()
		.map(Ok);

	let (membership, visibility, state, events, account_data) =
		try_join5(membership, visibility, state, events, account_data)
			.boxed()
			.await?;

	Ok(Response {
		room_id: room_id.to_owned(),
		membership,
		visibility: visibility.into(),
		account_data: Some(account_data),
		state: state.into(),
		messages: PaginationChunk {
			start: events
				.first()
				.map(at!(0))
				.as_ref()
				.map(ToString::to_string),

			end: events
				.last()
				.map(at!(0))
				.as_ref()
				.map(ToString::to_string)
				.unwrap_or_default(),

			chunk: events
				.into_iter()
				.map(at!(1))
				.map(Event::into_format)
				.collect(),
		}
		.into(),
	})
}
