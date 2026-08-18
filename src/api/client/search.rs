use std::collections::BTreeMap;

use axum::extract::State;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future::join};
use ruma::{
	OwnedRoomId, RoomId, UInt, UserId,
	api::client::search::search_events::{
		self,
		v3::{
			Criteria, EventContext, EventContextResult, ResultCategories, ResultRoomEvents,
			SearchResult,
		},
	},
	events::AnyStateEvent,
	serde::Raw,
};
use search_events::v3::{Request, Response};
use tuwunel_core::{
	Err, Result, at, is_true,
	matrix::Event,
	result::FlatOk,
	utils::{
		IterStream,
		option::OptionExt,
		stream::{ReadyExt, TryIgnore, WidebandExt},
	},
};
use tuwunel_service::{
	Services,
	rooms::{search::RoomQuery, timeline::PdusIterItem},
};

use crate::{Ruma, client::message::visibility_filter};

type RoomStates = BTreeMap<OwnedRoomId, RoomState>;
type RoomState = Vec<Raw<AnyStateEvent>>;

const LIMIT_DEFAULT: usize = 10;
const LIMIT_MAX: usize = 100;
const CONTEXT_MAX: usize = 20;
const BATCH_MAX: usize = 20;

/// # `POST /_matrix/client/r0/search`
///
/// Searches rooms for messages.
///
/// - Only works if the user is currently joined to the room (TODO: Respect
///   history visibility)
pub(crate) async fn search_events_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let sender_user = body.sender_user();
	let next_batch = body.next_batch.as_deref();
	let room_events = body
		.search_categories
		.room_events
		.as_ref()
		.map_async(|criteria| category_room_events(&services, sender_user, next_batch, criteria))
		.await
		.transpose()?;

	Ok(Response {
		search_categories: ResultCategories {
			room_events: room_events.unwrap_or_default(),
		},
	})
}

#[expect(clippy::map_unwrap_or)]
async fn category_room_events(
	services: &Services,
	sender_user: &UserId,
	next_batch: Option<&str>,
	criteria: &Criteria,
) -> Result<ResultRoomEvents> {
	let filter = &criteria.filter;

	let limit: usize = filter
		.limit
		.map(TryInto::try_into)
		.flat_ok()
		.unwrap_or(LIMIT_DEFAULT)
		.min(LIMIT_MAX);

	let next_batch: usize = next_batch
		.map(str::parse)
		.transpose()?
		.unwrap_or(0)
		.min(limit.saturating_mul(BATCH_MAX));

	let rooms = filter
		.rooms
		.clone()
		.map(IntoIterator::into_iter)
		.map(IterStream::stream)
		.map(StreamExt::boxed)
		.unwrap_or_else(|| {
			services
				.state_cache
				.rooms_joined(sender_user)
				.map(ToOwned::to_owned)
				.boxed()
		});

	let results: Vec<_> = rooms
		.filter_map(async |room_id| {
			check_room_visible(services, sender_user, &room_id, criteria)
				.await
				.is_ok()
				.then_some(room_id)
		})
		.filter_map(async |room_id| {
			let query = RoomQuery {
				room_id: &room_id,
				user_id: Some(sender_user),
				criteria,
				skip: next_batch,
				limit,
			};

			let (count, results) = services.search.search_pdus(&query).await.ok()?;

			results
				.collect::<Vec<_>>()
				.map(|results| (room_id.clone(), count, results))
				.map(Some)
				.await
		})
		.collect()
		.await;

	let total: UInt = results
		.iter()
		.fold(0, |a: usize, (_, count, _)| a.saturating_add(*count))
		.try_into()?;

	let state: RoomStates = results
		.iter()
		.stream()
		.ready_filter(|_| criteria.include_state.is_some_and(is_true!()))
		.filter_map(async |(room_id, ..)| {
			procure_room_state(services, room_id)
				.map_ok(|state| (room_id.clone(), state))
				.await
				.ok()
		})
		.collect()
		.await;

	let results: Vec<SearchResult> = results
		.into_iter()
		.map(at!(2))
		.flatten()
		.stream()
		.map(Event::into_pdu)
		.wide_then(async |pdu| {
			let context =
				event_context(services, sender_user, &pdu, &criteria.event_context).await;

			let pdu = services
				.pdu_metadata
				.bundle_aggregations(sender_user, pdu)
				.await;

			SearchResult {
				rank: None,
				result: Some(pdu.into_format()),
				context,
			}
		})
		.collect()
		.await;

	let highlights = criteria
		.search_term
		.split_terminator(|c: char| !c.is_alphanumeric())
		.map(str::to_lowercase)
		.collect();

	let next_batch = (results.len() >= limit)
		.then_some(next_batch.saturating_add(results.len()))
		.as_ref()
		.map(ToString::to_string);

	Ok(ResultRoomEvents {
		count: Some(total),
		next_batch,
		results,
		state,
		highlights,
		groups: Default::default(), // TODO
	})
}

async fn event_context<E>(
	services: &Services,
	sender_user: &UserId,
	pdu: &E,
	event_context: &EventContext,
) -> EventContextResult
where
	E: Event,
{
	// An absent event_context deserializes to the default 5/5; treat that as no
	// request.
	if event_context.is_default() {
		return EventContextResult::default();
	}

	let Ok(base_count) = services
		.timeline
		.get_pdu_count(pdu.event_id())
		.await
	else {
		return EventContextResult::default();
	};

	let room_id = pdu.room_id();
	let bounded = |limit: UInt| limit.try_into().unwrap_or(0).min(CONTEXT_MAX);
	let before_limit = bounded(event_context.before_limit);
	let after_limit = bounded(event_context.after_limit);

	let events_before = collect_context_half(
		services,
		services
			.timeline
			.pdus_rev(Some(sender_user), room_id, Some(base_count)),
		sender_user,
		before_limit,
	);

	let events_after = collect_context_half(
		services,
		services
			.timeline
			.pdus(Some(sender_user), room_id, Some(base_count)),
		sender_user,
		after_limit,
	);

	let (events_before, events_after) = join(events_before, events_after).await;

	let start = events_before
		.last()
		.map(at!(0))
		.or(Some(base_count))
		.as_ref()
		.map(ToString::to_string);

	let end = events_after
		.last()
		.map(at!(0))
		.or_else(|| Some(base_count.saturating_add(1)))
		.as_ref()
		.map(ToString::to_string);

	let events_before = events_before
		.into_iter()
		.map(at!(1))
		.map(Event::into_format)
		.collect();

	let events_after = events_after
		.into_iter()
		.map(at!(1))
		.map(Event::into_format)
		.collect();

	EventContextResult {
		start,
		end,
		events_before,
		events_after,
		profile_info: BTreeMap::new(),
	}
}

async fn collect_context_half<'a, S>(
	services: &'a Services,
	pdus: S,
	sender_user: &'a UserId,
	take: usize,
) -> Vec<PdusIterItem>
where
	S: Stream<Item = Result<PdusIterItem>> + Send + 'a,
{
	pdus.ignore_err()
		.wide_filter_map(|item| visibility_filter(services, item, sender_user))
		.take(take)
		.wide_then(async |(count, pdu)| {
			let pdu = services
				.pdu_metadata
				.bundle_aggregations(sender_user, pdu)
				.await;

			(count, pdu)
		})
		.collect()
		.await
}

async fn procure_room_state(services: &Services, room_id: &RoomId) -> Result<RoomState> {
	let state = services
		.state_accessor
		.room_state_full_pdus(room_id)
		.map_ok(Event::into_format)
		.try_collect()
		.await?;

	Ok(state)
}

async fn check_room_visible(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	search: &Criteria,
) -> Result {
	let check_visible = search.filter.rooms.is_some();
	let check_state = check_visible && search.include_state.is_some_and(is_true!());

	let is_joined = !check_visible
		|| services
			.state_cache
			.is_joined(user_id, room_id)
			.await;

	let state_visible = !check_state
		|| services
			.state_accessor
			.user_can_see_state_events(user_id, room_id)
			.await;

	if !is_joined || !state_visible {
		return Err!(Request(Forbidden("You don't have permission to view {room_id:?}")));
	}

	Ok(())
}
