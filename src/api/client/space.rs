use std::{collections::VecDeque, str::FromStr};

use axum::extract::State;
use conduwuit::{checked, pdu::ShortRoomId, utils::stream::IterStream};
use futures::{StreamExt, TryFutureExt};
use ruma::{
	api::client::{error::ErrorKind, space::get_hierarchy},
	OwnedRoomId, OwnedServerName, RoomId, UInt, UserId,
};
use service::{
	rooms::spaces::{get_parent_children_via, summary_to_chunk, SummaryAccessibility},
	Services,
};

use crate::{service::rooms::spaces::PaginationToken, Error, Result, Ruma};

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
			return Err(Error::BadRequest(
				ErrorKind::InvalidParam,
				"suggested_only and max_depth cannot change on paginated requests",
			));
		}
	}

	get_client_hierarchy(
		&services,
		body.sender_user(),
		&body.room_id,
		limit.try_into().unwrap_or(10),
		key.map_or(vec![], |token| token.short_room_ids),
		max_depth.into(),
		body.suggested_only,
	)
	.await
}

async fn get_client_hierarchy(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	limit: usize,
	short_room_ids: Vec<ShortRoomId>,
	max_depth: u64,
	suggested_only: bool,
) -> Result<get_hierarchy::v1::Response> {
	let mut parents = VecDeque::new();

	// Don't start populating the results if we have to start at a specific room.
	let mut populate_results = short_room_ids.is_empty();

	let mut stack = vec![vec![(room_id.to_owned(), match room_id.server_name() {
		| Some(server_name) => vec![server_name.into()],
		| None => vec![],
	})]];

	let mut results = Vec::with_capacity(limit);

	while let Some((current_room, via)) = { next_room_to_traverse(&mut stack, &mut parents) } {
		if results.len() >= limit {
			break;
		}

		match (
			services
				.rooms
				.spaces
				.get_summary_and_children_client(&current_room, suggested_only, sender_user, &via)
				.await?,
			current_room == room_id,
		) {
			| (Some(SummaryAccessibility::Accessible(summary)), _) => {
				let mut children: Vec<(OwnedRoomId, Vec<OwnedServerName>)> =
					get_parent_children_via(&summary, suggested_only)
						.into_iter()
						.filter(|(room, _)| parents.iter().all(|parent| parent != room))
						.rev()
						.collect();

				if populate_results {
					results.push(summary_to_chunk(*summary.clone()));
				} else {
					children = children
						.iter()
						.rev()
						.stream()
						.skip_while(|(room, _)| {
							services
								.rooms
								.short
								.get_shortroomid(room)
								.map_ok(|short| Some(&short) != short_room_ids.get(parents.len()))
								.unwrap_or_else(|_| false)
						})
						.map(Clone::clone)
						.collect::<Vec<(OwnedRoomId, Vec<OwnedServerName>)>>()
						.await
						.into_iter()
						.rev()
						.collect();

					if children.is_empty() {
						return Err(Error::BadRequest(
							ErrorKind::InvalidParam,
							"Room IDs in token were not found.",
						));
					}

					// We have reached the room after where we last left off
					let parents_len = parents.len();
					if checked!(parents_len + 1)? == short_room_ids.len() {
						populate_results = true;
					}
				}

				let parents_len: u64 = parents.len().try_into()?;
				if !children.is_empty() && parents_len < max_depth {
					parents.push_back(current_room.clone());
					stack.push(children);
				}
				// Root room in the space hierarchy, we return an error
				// if this one fails.
			},
			| (Some(SummaryAccessibility::Inaccessible), true) => {
				return Err(Error::BadRequest(
					ErrorKind::forbidden(),
					"The requested room is inaccessible",
				));
			},
			| (None, true) => {
				return Err(Error::BadRequest(
					ErrorKind::forbidden(),
					"The requested room was not found",
				));
			},
			// Just ignore other unavailable rooms
			| (None | Some(SummaryAccessibility::Inaccessible), false) => (),
		}
	}

	Ok(get_hierarchy::v1::Response {
		next_batch: if let Some((room, _)) = next_room_to_traverse(&mut stack, &mut parents) {
			parents.pop_front();
			parents.push_back(room);

			let next_short_room_ids: Vec<_> = parents
				.iter()
				.stream()
				.filter_map(|room_id| async move {
					services.rooms.short.get_shortroomid(room_id).await.ok()
				})
				.collect()
				.await;

			(next_short_room_ids != short_room_ids && !next_short_room_ids.is_empty()).then(
				|| {
					PaginationToken {
						short_room_ids: next_short_room_ids,
						limit: UInt::new(max_depth)
							.expect("When sent in request it must have been valid UInt"),
						max_depth: UInt::new(max_depth)
							.expect("When sent in request it must have been valid UInt"),
						suggested_only,
					}
					.to_string()
				},
			)
		} else {
			None
		},
		rooms: results,
	})
}

fn next_room_to_traverse(
	stack: &mut Vec<Vec<(OwnedRoomId, Vec<OwnedServerName>)>>,
	parents: &mut VecDeque<OwnedRoomId>,
) -> Option<(OwnedRoomId, Vec<OwnedServerName>)> {
	while stack.last().is_some_and(Vec::is_empty) {
		stack.pop();
		parents.pop_back();
	}

	stack.last_mut().and_then(Vec::pop)
}
