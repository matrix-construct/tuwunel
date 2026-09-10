use axum::extract::State;
use futures::{
	FutureExt, TryFutureExt, TryStreamExt,
	future::{ok, try_join, try_join4},
};
use ruma::{
	RoomId,
	api::client::room::initial_sync::v3::{PaginationChunk, Request, Response},
	events::{
		AnyRawAccountDataEvent,
		StateEventType::RoomMember,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use tuwunel_core::{
	Event, Result, at, err, extract_variant,
	matrix::{Pdu, PduCount},
	utils::{
		BoolExt, TryReadyExt,
		result::NotFound,
		stream::{TryTools, TryWidebandExt},
	},
};
use tuwunel_service::rooms::short::ShortStateHash;

use crate::{Ruma, client::message::visibility_filter};

const LIMIT_MAX: usize = 50;

/// GET `/_matrix/client/v3/rooms/{roomId}/initialSync`
pub(crate) async fn room_initial_sync_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let room_id = &body.room_id;
	let sender_user = body.sender_user();

	// `user_membership` uses `Ban` when a once-joined user's left row was forgotten.
	let cached_membership = services
		.state_cache
		.user_membership(sender_user, room_id)
		.await;

	matches!(cached_membership.as_ref(), Some(MembershipState::Ban))
		.is_false()
		.ok_or_else(|| err!(Request(Forbidden("No room preview available."))))?;

	services
		.state_accessor
		.user_can_see_state_events(sender_user, room_id)
		.await
		.ok_or_else(|| err!(Request(Forbidden("No room preview available."))))?;

	let current_shortstatehash = services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	let member = services
		.state_accessor
		.state_get(current_shortstatehash, &RoomMember, sender_user.as_str())
		.await
		.optional()?;

	let membership = member
		.as_ref()
		.map(Event::get_content)
		.transpose()?
		.map(|content: RoomMemberEventContent| content.membership);

	let next_batch = services.globals.current_count();
	let departure = membership
		.as_ref()
		.filter(|membership| matches!(membership, MembershipState::Leave | MembershipState::Ban))
		.zip(member.as_ref())
		.map(|(membership, pdu)| {
			departure_snapshot(
				&services,
				room_id,
				pdu,
				membership.to_owned(),
				current_shortstatehash,
			)
			.left_future()
		});

	let current_snapshot = ok((PduCount::Normal(next_batch), current_shortstatehash, membership));
	let (timeline_end, shortstatehash, membership) = departure
		.unwrap_or_else(|| current_snapshot.right_future())
		.await?;

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

	let (visibility, state, events, account_data) = try_join4(visibility, state, events, account_data)
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

async fn departure_snapshot(
	services: &crate::State,
	room_id: &RoomId,
	pdu: &Pdu,
	membership: MembershipState,
	current_shortstatehash: ShortStateHash,
) -> Result<(PduCount, ShortStateHash, Option<MembershipState>)> {
	let timeline_end = services.timeline.get_pdu_count(pdu.event_id());
	let latest_count = services
		.timeline
		.last_timeline_count(None, room_id, None);

	let (timeline_end, latest_count) = try_join(timeline_end, latest_count).await?;

	let shortstatehash = (latest_count == timeline_end)
		.then(|| ok(current_shortstatehash).left_future())
		.unwrap_or_else(|| {
			services
				.timeline
				.next_shortstatehash(room_id, timeline_end)
				.right_future()
		})
		.await?;

	Ok((timeline_end, shortstatehash, Some(membership)))
}
