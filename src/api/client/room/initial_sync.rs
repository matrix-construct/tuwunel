use axum::extract::State;
use futures::{FutureExt, TryFutureExt, TryStreamExt, future::try_join4};
use ruma::{
	api::client::room::initial_sync::v3::{PaginationChunk, Request, Response},
	events::{
		AnyRawAccountDataEvent, StateEventType::RoomMember, room::member::RoomMemberEventContent,
	},
};
use tuwunel_core::{
	Result, at, err, extract_variant,
	utils::{
		BoolExt, OptionExt, TryReadyExt,
		result::NotFound,
		stream::{TryTools, TryWidebandExt},
	},
};
use tuwunel_matrix::{Event, PduCount};

use crate::{Ruma, visibility_filter};

const LIMIT_MAX: usize = 50;

/// GET `/_matrix/client/v3/rooms/{roomId}/initialSync`
pub(crate) async fn room_initial_sync_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let room_id = &body.room_id;
	let sender_user = body.sender_user();

	services
		.state_accessor
		.user_can_see_state_events(sender_user, room_id)
		.await
		.into_option()
		.ok_or_else(|| err!(Request(Forbidden("No room preview available."))))?;

	let current_shortstatehash = services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	let membership = services
		.state_accessor
		.state_get(current_shortstatehash, &RoomMember, sender_user.as_str())
		.await
		.optional()?
		.as_ref()
		.map(Event::get_content)
		.transpose()?
		.map(|content: RoomMemberEventContent| content.membership);

	let departure = services
		.state_accessor
		.departure_count(sender_user, room_id)
		.await?;

	let next_batch = services.globals.current_count();
	let timeline_end = departure.unwrap_or(PduCount::Normal(next_batch));
	let shortstatehash = departure
		.map_async(|count| {
			services
				.timeline
				.shortstatehash_after(room_id, count)
		})
		.await
		.transpose()?
		.unwrap_or(current_shortstatehash);

	let visibility = services.directory.visibility(room_id).map(Ok);
	let limit = body.limit.unwrap_or(LIMIT_MAX).min(LIMIT_MAX);
	let state = services
		.state_accessor
		.state_full_pdus_strict(shortstatehash)
		.map_ok(Event::into_format)
		.try_collect::<Vec<_>>();

	let events = services
		.timeline
		.pdus_rev(Some(sender_user), room_id, Some(timeline_end.saturating_add(1)))
		.wide_and_then(|item| visibility_filter(&services, item, sender_user).map(Ok))
		.ready_try_filter_map(Ok)
		.try_take(limit)
		.try_collect()
		.map_ok(|mut vec: Vec<_>| {
			vec.reverse();
			vec
		});

	let account_data = services
		.account_data
		.changes_since_fallible(
			Some(room_id),
			sender_user,
			0,
			Some(timeline_end.into_normal().into_unsigned()),
		)
		.ready_try_filter_map(|e| Ok(extract_variant!(e, AnyRawAccountDataEvent::Room)))
		.try_collect::<Vec<_>>();

	let (visibility, state, events, account_data) =
		try_join4(visibility, state, events, account_data)
			.boxed() // erase the state stream's higher-ranked event lifetime
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
				.map_or_else(|| timeline_end.to_string(), ToString::to_string),

			chunk: events
				.into_iter()
				.map(at!(1))
				.map(Event::into_format)
				.collect(),
		}
		.into(),
	})
}
