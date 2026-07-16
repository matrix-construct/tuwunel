use axum::extract::State;
use futures::StreamExt;
use ruma::{OwnedRoomId, UInt, api::Direction};
use synapse_admin_api::federation::destination_rooms::v1::{DestinationRoom, Request, Response};
use tuwunel_core::{
	Err, Result,
	utils::math::{ruma_from_usize, usize_from_ruma},
};

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/federation/destinations/{destination}/rooms`
///
/// The last-sent PDU stream ordering is not tracked per room; every row
/// reports 0.
pub(crate) async fn admin_destination_rooms_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	if services.globals.server_is_ours(&body.destination) {
		return Err!(Request(NotFound("Unknown destination")));
	}

	let rooms: Vec<OwnedRoomId> = services
		.state_cache
		.server_rooms(&body.destination)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	if rooms.is_empty()
		&& !services
			.federation
			.peer_has_failures(&body.destination)
			.await
	{
		return Err!(Request(NotFound("Unknown destination")));
	}

	let from = body.from.map_or(0, usize_from_ruma);
	let limit = body.limit.map_or(100, usize_from_ruma);
	let dir = body.dir.unwrap_or(Direction::Forward);
	let (rooms, total, next_token) = paginate(rooms, dir, from, limit);

	Ok(Response { rooms, total, next_token })
}

/// The `next_token` is the stringified next offset, emitted only while rows
/// remain past the returned page (Synapse's `str(from + len)` convention).
fn paginate(
	mut rooms: Vec<OwnedRoomId>,
	dir: Direction,
	from: usize,
	limit: usize,
) -> (Vec<DestinationRoom>, UInt, Option<String>) {
	rooms.sort_unstable();

	if matches!(dir, Direction::Backward) {
		rooms.reverse();
	}

	let total = rooms.len();

	let page: Vec<DestinationRoom> = rooms
		.into_iter()
		.skip(from)
		.take(limit)
		.map(|room_id| DestinationRoom {
			room_id,
			stream_ordering: UInt::from(0_u32),
		})
		.collect();

	let end = from.saturating_add(page.len());
	let next_token = (end < total).then(|| end.to_string());

	(page, ruma_from_usize(total), next_token)
}

#[cfg(test)]
mod tests {
	use ruma::{OwnedRoomId, api::Direction, owned_room_id, uint};

	use super::{DestinationRoom, paginate};

	fn rooms() -> Vec<OwnedRoomId> {
		vec![
			owned_room_id!("!c:example.com"),
			owned_room_id!("!a:example.com"),
			owned_room_id!("!b:example.com"),
		]
	}

	fn ids(page: &[DestinationRoom]) -> impl Iterator<Item = &str> {
		page.iter().map(|room| room.room_id.as_str())
	}

	#[test]
	fn forward_mid_window_emits_token() {
		let (page, total, next_token) = paginate(rooms(), Direction::Forward, 0, 2);

		assert!(ids(&page).eq(["!a:example.com", "!b:example.com"]));
		assert!(
			page.iter()
				.all(|room| room.stream_ordering == uint!(0))
		);
		assert_eq!(total, uint!(3));
		assert_eq!(next_token.as_deref(), Some("2"));
	}

	#[test]
	fn final_window_omits_token() {
		let (page, _, next_token) = paginate(rooms(), Direction::Forward, 2, 2);

		assert!(ids(&page).eq(["!c:example.com"]));
		assert_eq!(next_token, None);
	}

	#[test]
	fn from_past_end_is_empty() {
		let (page, total, next_token) = paginate(rooms(), Direction::Forward, 5, 2);

		assert!(page.is_empty());
		assert_eq!(total, uint!(3));
		assert_eq!(next_token, None);
	}

	#[test]
	fn zero_limit_holds_position() {
		let (page, _, next_token) = paginate(rooms(), Direction::Forward, 1, 0);

		assert!(page.is_empty());
		assert_eq!(next_token.as_deref(), Some("1"));
	}

	#[test]
	fn backward_reverses_before_windowing() {
		let (page, _, next_token) = paginate(rooms(), Direction::Backward, 0, 2);

		assert!(ids(&page).eq(["!c:example.com", "!b:example.com"]));
		assert_eq!(next_token.as_deref(), Some("2"));
	}
}
