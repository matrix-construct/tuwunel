use axum::extract::State;
use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{
		Either::{Left, Right},
		try_join3,
	},
};
use itertools::Itertools;
use ruma::{
	EventId, RoomId, UInt, UserId,
	api::{
		Direction,
		client::relations::{
			get_relating_events::{self, v1::Response},
			get_relating_events_with_rel_type, get_relating_events_with_rel_type_and_event_type,
		},
	},
	events::{TimelineEventType, relation::RelationType},
};
use tuwunel_core::{
	Err, Error, Result, at, err,
	utils::{
		BoolExt, extended,
		math::usize_from_ruma_bounded,
		result::FlatOk,
		stream::{IterStream, ReadyExt, WidebandExt},
	},
};
use tuwunel_matrix::{
	event::{Event, RelationTypeEqual},
	pdu::{Pdu, PduCount, PduId},
};
use tuwunel_service::{Services, rooms::pdu_metadata::RequesterQuery};

use crate::{Ruma, is_ignored_pdu};

/// The next level's targets and the events retained so far in a recursive walk.
type Level = (Vec<PduCount>, Vec<(PduCount, Pdu)>);

// Matrix v1.10 recommends traversing at least 3 levels of relationships.
const RECURSION_DEPTH: u32 = 3;

/// # `GET /_matrix/client/r0/rooms/{roomId}/relations/{eventId}/{relType}/{eventType}`
pub(crate) async fn get_relating_events_with_rel_type_and_event_type_route(
	State(services): State<crate::State>,
	body: Ruma<get_relating_events_with_rel_type_and_event_type::v1::Request>,
) -> Result<get_relating_events_with_rel_type_and_event_type::v1::Response> {
	paginate_relations_with_filter(
		&services,
		body.sender_user(),
		&body.room_id,
		&body.event_id,
		body.event_type.clone().into(),
		body.rel_type.clone().into(),
		body.from.as_deref(),
		body.to.as_deref(),
		body.limit,
		body.recurse,
		body.dir,
	)
	.await
	.map(|res| get_relating_events_with_rel_type_and_event_type::v1::Response {
		chunk: res.chunk,
		next_batch: res.next_batch,
		prev_batch: res.prev_batch,
		recursion_depth: res.recursion_depth,
	})
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/relations/{eventId}/{relType}`
pub(crate) async fn get_relating_events_with_rel_type_route(
	State(services): State<crate::State>,
	body: Ruma<get_relating_events_with_rel_type::v1::Request>,
) -> Result<get_relating_events_with_rel_type::v1::Response> {
	paginate_relations_with_filter(
		&services,
		body.sender_user(),
		&body.room_id,
		&body.event_id,
		None,
		body.rel_type.clone().into(),
		body.from.as_deref(),
		body.to.as_deref(),
		body.limit,
		body.recurse,
		body.dir,
	)
	.await
	.map(|res| get_relating_events_with_rel_type::v1::Response {
		chunk: res.chunk,
		next_batch: res.next_batch,
		prev_batch: res.prev_batch,
		recursion_depth: res.recursion_depth,
	})
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/relations/{eventId}`
pub(crate) async fn get_relating_events_route(
	State(services): State<crate::State>,
	body: Ruma<get_relating_events::v1::Request>,
) -> Result<Response> {
	paginate_relations_with_filter(
		&services,
		body.sender_user(),
		&body.room_id,
		&body.event_id,
		None,
		None,
		body.from.as_deref(),
		body.to.as_deref(),
		body.limit,
		body.recurse,
		body.dir,
	)
	.await
}

#[expect(clippy::too_many_arguments)]
#[tracing::instrument(
	name = "relations",
	level = "debug",
	skip_all,
	fields(room_id, target, from, to, dir, limit, recurse)
)]
async fn paginate_relations_with_filter(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	target: &EventId,
	filter_event_type: Option<TimelineEventType>,
	filter_rel_type: Option<RelationType>,
	from: Option<&str>,
	to: Option<&str>,
	limit: Option<UInt>,
	recurse: bool,
	dir: Direction,
) -> Result<Response> {
	let recursion_depth = recurse.then(|| UInt::from(RECURSION_DEPTH));
	let from: Option<PduCount> = from.map(str::parse).transpose()?;

	let to: Option<PduCount> = to.map(str::parse).flat_ok();

	let limit = limit.map_or(30, |limit| usize_from_ruma_bounded(limit, 30, 100));

	let target_event_id: &EventId = target;

	let target = services
		.timeline
		.get_pdu_id(target)
		.map_ok(PduId::from)
		.map_ok(Ok::<_, Error>);

	let visible = services
		.state_accessor
		.user_can_see_state_events(sender_user, room_id)
		.map(|visible| {
			visible
				.into_option()
				.ok_or_else(|| err!(Request(Forbidden("You cannot view this room."))))
		});

	let shortroomid = services.short.get_shortroomid(room_id);

	let (shortroomid, target, ()) = try_join3(shortroomid, target, visible).await?;

	let empty = || {
		Ok(Response {
			recursion_depth,
			..Response::new(Vec::new())
		})
	};

	let Ok(target) = target else {
		return empty();
	};

	if shortroomid != target.shortroomid {
		return Err!(Request(NotFound("Event not found in room.")));
	}

	if let Ok(target_pdu) = services.timeline.get_pdu(target_event_id).await
		&& is_ignored_pdu(services, &target_pdu, sender_user).await
	{
		return Err!(HttpJson(NOT_FOUND, {
			"errcode": "M_SENDER_IGNORED",
			"error": "You have ignored the user that sent this event",
			"sender": target_pdu.sender().as_str(),
		}));
	}

	let PduCount::Normal(_) = target.count else {
		return empty();
	};

	let ignored = services.users.ignored_users(sender_user).await;

	let filter = |(count, pdu): &(PduCount, Pdu)| {
		matches!(count, PduCount::Normal(_))
			&& filter_event_type
				.as_ref()
				.is_none_or(|kind| kind == pdu.kind())
			&& filter_rel_type
				.as_ref()
				.is_none_or(|rel_type| rel_type.relation_type_equal(pdu))
	};

	let query = RequesterQuery {
		shortroomid,
		target: target.count,
		from,
		to,
		dir,
		user_id: sender_user,
		ignored: ignored.as_ref(),
	};

	let relations = if recurse {
		let relations = walk_relations(services, query, &filter)
			.await
			.stream();

		Left(relations)
	} else {
		let relations = services
			.pdu_metadata
			.requester_relations(query)
			.ready_filter(&filter);

		Right(relations)
	};

	let mut events = relations
		.wide_filter_map(async |(count, pdu)| {
			services
				.state_accessor
				.user_can_see_event(sender_user, &pdu)
				.await
				.then_some((count, pdu))
		})
		.take(limit.saturating_add(1))
		.collect::<Vec<_>>()
		.await;

	let more = events.len() > limit;
	events.truncate(limit);

	Ok(Response {
		recursion_depth,

		next_batch: events
			.last()
			.filter(|_| more)
			.map(at!(0))
			.as_ref()
			.map(ToString::to_string),

		prev_batch: events
			.first()
			.map(at!(0))
			.or(from)
			.as_ref()
			.map(ToString::to_string),

		chunk: events
			.into_iter()
			.map(at!(1))
			.stream()
			.wide_then(|pdu| {
				services
					.pdu_metadata
					.bundle_aggregations(sender_user, pdu)
			})
			.map(Event::into_format)
			.collect()
			.await,
	})
}

/// Walks matching relations by level and returns the requested window in count order.
///
/// Descent ignores from/to because an outside parent can have children inside.
/// Only matching children are descended through, and only those inside are retained.
async fn walk_relations(
	services: &Services,
	query: RequesterQuery<'_>,
	filter: &(impl Fn(&(PduCount, Pdu)) -> bool + Sync),
) -> impl Iterator<Item = (PduCount, Pdu)> {
	let (_, events) = (1..=RECURSION_DEPTH)
		.stream()
		.fold((vec![query.target], Vec::new()), async |state, depth| {
			walk_level(services, query, filter, state, depth).await
		})
		.await;

	events
		.into_iter()
		.sorted_unstable_by(|(a, _), (b, _)| match query.dir {
			| Direction::Forward => a.cmp(b),
			| Direction::Backward => b.cmp(a),
		})
}

async fn walk_level(
	services: &Services,
	query: RequesterQuery<'_>,
	filter: &(impl Fn(&(PduCount, Pdu)) -> bool + Sync),
	(frontier, events): Level,
	depth: u32,
) -> Level {
	let accumulate =
		|(frontier, events), (child, event)| (extended(frontier, child), extended(events, event));

	frontier
		.into_iter()
		.stream()
		.flat_map(|target| {
			services
				.pdu_metadata
				.requester_relations(RequesterQuery { target, from: None, to: None, ..query })
		})
		.ready_filter(filter)
		.map(|(count, pdu)| {
			let child = depth.lt(&RECURSION_DEPTH).then_some(count);

			let event = query
				.from
				.is_none_or(|from| from.is_before(count, query.dir))
				.then_some((count, pdu))
				.filter(|(count, _)| {
					query
						.to
						.is_none_or(|to| count.is_before(to, query.dir))
				});

			(child, event)
		})
		.ready_fold((Vec::new(), events), accumulate)
		.await
}
