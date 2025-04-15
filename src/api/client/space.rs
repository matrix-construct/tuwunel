use std::{
	collections::{BTreeSet, VecDeque},
	str::FromStr,
};

use axum::extract::State;
use conduwuit::{
	Err, Result,
	utils::{future::TryExtExt, stream::IterStream},
};
use conduwuit_service::{
	Services,
	rooms::spaces::{
		PaginationToken, SummaryAccessibility, get_parent_children_via, summary_to_chunk,
	},
};
use futures::{StreamExt, TryFutureExt, future::OptionFuture};
use ruma::{
	OwnedRoomId, OwnedServerName, RoomId, UInt, UserId, api::client::space::get_hierarchy,
};

use crate::Ruma;

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

	// Should prevent unexpeded behaviour in (bad) clients
	if let Some(ref token) = key {
		if token.suggested_only != body.suggested_only || token.max_depth != max_depth {
			return Err!(Request(InvalidParam(
				"suggested_only and max_depth cannot change on paginated requests"
			)));
		}
	}

	get_client_hierarchy(
		&services,
		body.sender_user(),
		&body.room_id,
		limit.try_into().unwrap_or(10),
		max_depth.try_into().unwrap_or(usize::MAX),
		body.suggested_only,
		key.as_ref()
			.into_iter()
			.flat_map(|t| t.short_room_ids.iter()),
	)
	.await
}

async fn get_client_hierarchy<'a, ShortRoomIds>(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	limit: usize,
	max_depth: usize,
	suggested_only: bool,
	short_room_ids: ShortRoomIds,
) -> Result<get_hierarchy::v1::Response>
where
	ShortRoomIds: Iterator<Item = &'a u64> + Clone + Send + Sync + 'a,
{
	type Via = Vec<OwnedServerName>;
	type Entry = (OwnedRoomId, Via);
	type Rooms = VecDeque<Entry>;

	let mut queue: Rooms = [(
		room_id.to_owned(),
		room_id
			.server_name()
			.map(ToOwned::to_owned)
			.into_iter()
			.collect(),
	)]
	.into();

	let mut rooms = Vec::with_capacity(limit);
	let mut parents = BTreeSet::new();
	while let Some((current_room, via)) = queue.pop_front() {
		let summary = services
			.rooms
			.spaces
			.get_summary_and_children_client(&current_room, suggested_only, sender_user, &via)
			.await?;

		match (summary, current_room == room_id) {
			| (None | Some(SummaryAccessibility::Inaccessible), false) => {
				// Just ignore other unavailable rooms
			},
			| (None, true) => {
				return Err!(Request(Forbidden("The requested room was not found")));
			},
			| (Some(SummaryAccessibility::Inaccessible), true) => {
				return Err!(Request(Forbidden("The requested room is inaccessible")));
			},
			| (Some(SummaryAccessibility::Accessible(summary)), _) => {
				let populate = parents.len() >= short_room_ids.clone().count();

				let mut children: Vec<Entry> = get_parent_children_via(&summary, suggested_only)
					.filter(|(room, _)| !parents.contains(room))
					.rev()
					.map(|(key, val)| (key, val.collect()))
					.collect();

				if !populate {
					children = children
						.iter()
						.rev()
						.stream()
						.skip_while(|(room, _)| {
							services
								.rooms
								.short
								.get_shortroomid(room)
								.map_ok(|short| {
									Some(&short) != short_room_ids.clone().nth(parents.len())
								})
								.unwrap_or_else(|_| false)
						})
						.map(Clone::clone)
						.collect::<Vec<Entry>>()
						.await
						.into_iter()
						.rev()
						.collect();
				}

				if populate {
					rooms.push(summary_to_chunk(summary.clone()));
				} else if queue.is_empty() && children.is_empty() {
					return Err!(Request(InvalidParam("Room IDs in token were not found.")));
				}

				parents.insert(current_room.clone());
				if rooms.len() >= limit {
					break;
				}

				if parents.len() > max_depth {
					continue;
				}

				queue.extend(children);
			},
		}
	}

	let next_batch: OptionFuture<_> = queue
		.pop_front()
		.map(|(room, _)| async move {
			parents.insert(room);

			let next_short_room_ids: Vec<_> = parents
				.iter()
				.stream()
				.filter_map(|room_id| services.rooms.short.get_shortroomid(room_id).ok())
				.collect()
				.await;

			(next_short_room_ids.iter().ne(short_room_ids) && !next_short_room_ids.is_empty())
				.then_some(PaginationToken {
					short_room_ids: next_short_room_ids,
					limit: limit.try_into().ok()?,
					max_depth: max_depth.try_into().ok()?,
					suggested_only,
				})
				.as_ref()
				.map(PaginationToken::to_string)
		})
		.into();

	Ok(get_hierarchy::v1::Response {
		next_batch: next_batch.await.flatten(),
		rooms,
	})
}
