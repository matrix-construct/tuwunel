use std::{
	collections::{BTreeSet, VecDeque},
	convert::identity,
	str::FromStr,
};

use axum::extract::State;
use futures::{
	StreamExt,
	future::ready,
	stream::{once, unfold},
};
use ruma::{
	OwnedRoomId, OwnedServerName, RoomId, UInt, UserId, api::client::space::get_hierarchy,
};
use tuwunel_core::{
	Err, Result, debug_error, error,
	smallvec::SmallVec,
	trace,
	utils::{
		BoolExt,
		stream::{IterStream, ReadyExt, WidebandExt},
	},
};
use tuwunel_service::{
	Services,
	rooms::{
		short::ShortRoomId,
		spaces::{
			Accessibility, Identifier, PaginationToken, get_parent_children_via,
			is_summary_serializable, summary_to_chunk,
		},
	},
};

use crate::Ruma;

/// Inputs for [`get_client_hierarchy`], shared by the client-server
/// `/hierarchy` route and the admin room-hierarchy endpoint.
pub(crate) struct HierarchyArgs<'a> {
	pub sender_user: &'a UserId,
	pub room_id: &'a RoomId,
	pub limit: usize,
	pub max_depth: usize,
	pub suggested_only: bool,
	pub skip_room_ids: &'a [ShortRoomId],

	/// Traverse from the server's perspective without federating and skip the
	/// per-user visibility gate, for admin callers.
	pub bypass_visibility: bool,
}

/// # `GET /_matrix/client/v1/rooms/{room_id}/hierarchy`
///
/// Paginates over the space tree in a depth-first manner to locate child rooms
/// of a given space.
pub(crate) async fn get_hierarchy_route(
	State(services): State<crate::State>,
	body: Ruma<get_hierarchy::v1::Request>,
) -> Result<get_hierarchy::v1::Response> {
	let limit = body
		.limit
		.unwrap_or_else(|| UInt::from(10_u32))
		.min(UInt::from(100_u32));

	let max_depth = body
		.max_depth
		.unwrap_or_else(|| UInt::from(3_u32))
		.min(UInt::from(10_u32));

	let key = body
		.from
		.as_ref()
		.and_then(|s| PaginationToken::from_str(s).ok());

	// Should prevent unexpected behaviour in (bad) clients
	if let Some(ref token) = key
		&& (token.suggested_only != body.suggested_only || token.max_depth != max_depth)
	{
		return Err!(Request(InvalidParam(
			"suggested_only and max_depth cannot change on paginated requests"
		)));
	}

	get_client_hierarchy(&services, HierarchyArgs {
		sender_user: body.sender_user(),
		room_id: &body.room_id,
		limit: limit.try_into().unwrap_or(10),
		max_depth: max_depth.try_into().unwrap_or(usize::MAX),
		suggested_only: body.suggested_only,
		skip_room_ids: key
			.as_ref()
			.map(|t| t.short_room_ids.as_slice())
			.unwrap_or_default(),
		bypass_visibility: false,
	})
	.await
}

pub(crate) async fn get_client_hierarchy(
	services: &Services,
	args: HierarchyArgs<'_>,
) -> Result<get_hierarchy::v1::Response> {
	type Via = SmallVec<[OwnedServerName; 1]>;
	type QueueItem = (OwnedRoomId, Via, usize);

	let HierarchyArgs {
		sender_user,
		room_id,
		limit,
		max_depth,
		suggested_only,
		skip_room_ids,
		bypass_visibility,
	} = args;

	// Admin callers traverse from the server's perspective and never federate,
	// so remote children surface as holes and the visibility gate is skipped.
	let sender = match bypass_visibility {
		| true => Identifier::ServerName(services.globals.server_name()),
		| false => Identifier::UserId(sender_user),
	};

	// Fetch the root room up front so we can return precise errors for
	// inaccessibility rather than silently dropping it.
	let root_via: Via = match bypass_visibility {
		| true => Via::new(),
		| false => room_id
			.server_name()
			.map(ToOwned::to_owned)
			.into_iter()
			.collect(),
	};

	let root_summary = match services
		.spaces
		.get_summary_and_children(room_id, &sender, &root_via)
		.await
	{
		| Err(e) => {
			debug_error!(?room_id, "space hierarchy root: {e}");
			return Err(e);
		},
		| Ok(Accessibility::Inaccessible) => {
			return Err!(Request(Forbidden(debug_error!("The requested room is inaccessible."))));
		},
		| Ok(Accessibility::Accessible(s)) => s,
	};

	// Seed the depth-first traversal: root is already visited; its children
	// form the initial queue at depth 1.
	let initial_queue: VecDeque<QueueItem> = max_depth
		.gt(&0)
		.then(|| {
			get_parent_children_via(&root_summary, suggested_only)
				.filter(|(room_id_, _)| room_id.ne(room_id_))
				.map(|(room_id, via)| {
					let via = match bypass_visibility {
						| true => Via::new(),
						| false => via.collect(),
					};

					(room_id, via, 1_usize)
				})
		})
		.into_iter()
		.flatten()
		.collect();

	// Short IDs of rooms already returned on previous pages; skip them in output
	// but still traverse their children to preserve depth-first order.
	let skip_ids: BTreeSet<ShortRoomId> = skip_room_ids.iter().copied().collect();

	let initial_state = (initial_queue, BTreeSet::from([room_id.to_owned()]));

	// Stream all accessible rooms in depth-first order: root first, then
	// descendants discovered by unfolding the queue.
	let rooms = once(ready(Some(root_summary)))
		.chain(unfold(initial_state, async |(mut queue, mut visited)| {
			let (current_room, via, depth) = queue.pop_front()?;

			// Cycle guard: a room reachable via multiple parents is only
			// visited (and queued for children) once.
			if visited.contains(&current_room) {
				return Some((None, (queue, visited)));
			}

			match services
				.spaces
				.get_summary_and_children(&current_room, &sender, &via)
				.await
			{
				| Err(e) if !e.is_not_found() => {
					error!(?current_room, ?depth, "space child error: {e}");

					Some((None, (queue, visited)))
				},
				| Err(_) | Ok(Accessibility::Inaccessible) => {
					trace!(?current_room, ?depth, "child inaccessible or not found");

					Some((None, (queue, visited)))
				},
				| Ok(Accessibility::Accessible(s)) => {
					visited.insert(current_room);

					// Enqueue children only while within the depth budget.
					if depth < max_depth {
						get_parent_children_via(&s, suggested_only)
							.filter(|(child, _)| !visited.contains(child))
							.for_each(|(child, via)| {
								let via = match bypass_visibility {
									| true => Via::new(),
									| false => via.collect(),
								};

								queue.push_back((child, via, depth.saturating_add(1)));
							});
					}

					Some((Some(s), (queue, visited)))
				},
			}
		}))
		.ready_filter_map(identity)
		.wide_filter_map(async |summary| {
			skip_ids
				.is_empty()
				.is_false()
				.then_async(async || {
					services
						.short
						.get_shortroomid(&summary.summary.room_id)
						.await
						.ok()
						.filter(|shortid| skip_ids.contains(shortid))
				})
				.await
				.flatten()
				.is_none()
				.then_some(summary)
				.filter(is_summary_serializable)
				.map(summary_to_chunk)
		})
		.take(limit)
		.collect::<Vec<_>>()
		.await;

	// If we filled the page, produce a continuation token encoding every room
	// emitted so far (previous pages + this page). The next request skips all
	// of them and resumes from the next position in the traversal order.
	let next_batch = (limit > 0 && rooms.len() >= limit)
		.then_async(async || {
			let next_skip = skip_room_ids
				.iter()
				.copied()
				.stream()
				.chain(rooms.iter().stream().then(async |chunk| {
					// `get_or_create_shortroomid` is used (not `get_shortroomid`) because rooms
					// in a remote hierarchy our server has never touched have no shortroomid
					// allocated yet; `get_shortroomid` would return `Err` and the room would
					// silently fall out of the skip set, causing the next page to re-emit the
					// same rooms with the same token — an infinite loop.
					services
						.short
						.get_or_create_shortroomid(&chunk.summary.room_id)
						.await
				}))
				.collect::<Vec<_>>()
				.await;

			// Backstop against pagination loops: only return a token if the skip
			// set strictly grew. With `get_or_create_shortroomid` above this should
			// always hold when `rooms.len() >= limit`, but checking is cheap.
			(next_skip.len() > skip_room_ids.len()).then_some(PaginationToken {
				suggested_only,
				short_room_ids: next_skip,
				limit: limit.try_into().unwrap_or_default(),
				max_depth: max_depth.try_into().unwrap_or_default(),
			})
		})
		.await
		.flatten()
		.as_ref()
		.map(ToString::to_string);

	Ok(get_hierarchy::v1::Response { rooms, next_batch })
}
