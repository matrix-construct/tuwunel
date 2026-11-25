use axum::extract::State;
use futures::{
	StreamExt,
	future::Either::{Left, Right},
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
	Err, Result, at, err,
	utils::{
		extended,
		math::usize_from_ruma_bounded,
		result::FlatOk,
		stream::{IterStream, ReadyExt, WidebandExt},
	},
	warn,
};
use tuwunel_matrix::{
	event::{Event, RelationTypeEqual},
	pdu::{Pdu, PduCount, PduId},
};
use tuwunel_service::{Services, rooms::pdu_metadata::RequesterQuery};

use crate::{Ruma, is_ignored_pdu};

/// The next level's targets and the events retained so far in a recursive walk.
type Level = (Vec<PduCount>, Vec<(PduCount, Pdu)>);

/// How much of the room's history a requester may read through its relations.
///
/// The parent and every relation returned are judged under the same access, and
/// world-readable access serves relations without bundled aggregations.
#[derive(Clone, Copy)]
enum Access {
	/// The room admits or invites them, so each event is judged on its own.
	Room,

	/// They never joined, so only events sent while history was world-readable.
	WorldReadable,
}

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

	let (target, access) = {
		let (target, parent, access) = visible_parent(services, sender_user, room_id, target)
			.await
			.ok_or_else(|| err!(Request(NotFound("Event not found."))))?;

		if is_ignored_pdu(services, &parent, sender_user).await {
			return Err!(HttpJson(NOT_FOUND, {
				"errcode": "M_SENDER_IGNORED",
				"error": "You have ignored the user that sent this event",
				"sender": parent.sender().as_str(),
			}));
		}

		(target, access)
	};

	let PduCount::Normal(_) = target.count else {
		return Ok(Response {
			recursion_depth,
			..Response::new(Vec::new())
		});
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
		shortroomid: target.shortroomid,
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
			visible_to(services, sender_user, access, &pdu)
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
			.wide_filter_map(|pdu| served(services, sender_user, access, pdu))
			.map(Event::into_format)
			.collect()
			.await,
	})
}

/// Finds the parent event, its timeline position and the requester's access
/// when it is in the room and the requester may read it.
///
/// An unknown parent, one only stored as an outlier, one in another room, one
/// whose rows fail to read and a denied one all come back `None`, so the caller
/// refuses each with the one 404 the spec gives for "not found or no
/// permission".
async fn visible_parent(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	target: &EventId,
) -> Option<(PduId, Pdu, Access)> {
	let pdu_id = services.timeline.get_pdu_id(target).await.ok()?;

	let parent = services
		.timeline
		.get_pdu_from_id(&pdu_id)
		.await
		.ok()?;

	if parent.room_id() != room_id {
		return None;
	}

	let access = access(services, sender_user, room_id).await?;

	visible_to(services, sender_user, access, &parent)
		.await
		.then_some((pdu_id.into(), parent, access))
}

/// Decides how much of the room's history the requester may read, if any.
///
/// The room admits members, departed members and everyone while its history is
/// world-readable, and a pending invitee reads as well. If the room does not
/// admit them, one who never joined reads only world-readable history, and one
/// who once joined, as after a forget, reads none. A failed read narrows access.
async fn access(services: &Services, sender_user: &UserId, room_id: &RoomId) -> Option<Access> {
	let admitted = services
		.state_accessor
		.user_can_see_state_events(sender_user, room_id)
		.await
		|| services
			.state_cache
			.is_invited(sender_user, room_id)
			.await;

	if admitted {
		return Some(Access::Room);
	}

	matches!(
		services
			.state_cache
			.try_once_joined(sender_user, room_id)
			.await,
		Ok(false)
	)
	.then_some(Access::WorldReadable)
}

/// Decides whether the requester may see an event within their access.
///
/// World-readable access also needs the event sent while history was
/// world-readable, before its own visibility is judged.
async fn visible_to(
	services: &Services,
	sender_user: &UserId,
	access: Access,
	pdu: &Pdu,
) -> bool {
	let visible = services
		.state_accessor
		.user_can_see_event(sender_user, pdu);

	match access {
		| Access::Room => visible.await,
		| Access::WorldReadable =>
			services
				.state_accessor
				.is_world_readable_at(pdu)
				.await
				&& visible.await,
	}
}

/// Readies a relation for the requester within their access, or `None` to
/// withhold it.
///
/// World-readable access folds no aggregations, since each bundled child would
/// need its own visibility check against that access. If the erasure prune a
/// relation requires fails, the relation is withheld.
async fn served(
	services: &Services,
	sender_user: &UserId,
	access: Access,
	pdu: Pdu,
) -> Option<Pdu> {
	let metadata = &services.pdu_metadata;

	match access {
		| Access::Room => metadata
			.bundle_aggregations(sender_user, pdu)
			.await
			.into(),

		| Access::WorldReadable => metadata
			.unbundled(sender_user, pdu)
			.await
			.inspect_err(|error| warn!(%error, "Erasure prune failed; relation withheld."))
			.ok(),
	}
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
