use std::cmp::Ordering;

use axum::extract::State;
use futures::StreamExt;
use ruma::uint;
use synapse_admin_api::rooms::list_rooms::v1::{
	Request, Response, RoomDetails, RoomSortOrder, SortDirection,
};
use tuwunel_core::{
	Result,
	utils::stream::{BroadbandExt, ReadyExt},
};

use super::{room_row, usize_to_uint};
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/rooms`
///
/// Lists rooms known to the server with search, filtering, ordering, and
/// integer-offset pagination, mirroring Synapse's List Room API.
pub(crate) async fn admin_list_rooms_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let search_term = body.search_term.as_deref().map(str::to_lowercase);
	let order_by = body
		.order_by
		.clone()
		.unwrap_or(RoomSortOrder::Name);

	let backward = matches!(body.dir, Some(SortDirection::Backward));

	let mut rooms: Vec<RoomDetails> = services
		.metadata
		.iter_ids()
		.map(ToOwned::to_owned)
		.broad_then(async |room_id| room_row(&services, &room_id).await)
		.ready_filter(|room| {
			matches_search(room, search_term.as_deref())
				&& matches_public(room, body.public_rooms)
				&& matches_empty(room, body.empty_rooms)
		})
		.collect()
		.await;

	sort_rooms(&mut rooms, &order_by);

	if backward {
		rooms.reverse();
	}

	let total_rooms = usize_to_uint(rooms.len());
	let from = body.from.unwrap_or_else(|| uint!(0));
	let limit = body.limit.unwrap_or_else(|| uint!(100));

	let offset = usize::try_from(from).unwrap_or(usize::MAX);
	let window = usize::try_from(limit).unwrap_or(usize::MAX);
	let next_offset = offset.saturating_add(window);

	let next_batch = (next_offset < rooms.len()).then(|| usize_to_uint(next_offset));
	let prev_batch = (offset > 0).then(|| usize_to_uint(offset.saturating_sub(window)));

	let page: Vec<RoomDetails> = rooms
		.into_iter()
		.skip(offset)
		.take(window)
		.collect();

	Ok(Response {
		rooms: page,
		offset: usize_to_uint(offset),
		total_rooms,
		next_batch,
		prev_batch,
	})
}

/// Matches a room against the search term: the name and canonical alias by
/// case-insensitive substring, but the room id by exact equality.
fn matches_search(room: &RoomDetails, search_term: Option<&str>) -> bool {
	let Some(term) = search_term else {
		return true;
	};

	let name_hit = room
		.name
		.as_deref()
		.is_some_and(|name| name.to_lowercase().contains(term));

	let alias_hit = room
		.canonical_alias
		.as_ref()
		.is_some_and(|alias| alias.as_str().to_lowercase().contains(term));

	name_hit || alias_hit || room.room_id.as_str() == term
}

fn matches_public(room: &RoomDetails, public_rooms: Option<bool>) -> bool {
	public_rooms.is_none_or(|want| room.public == want)
}

fn matches_empty(room: &RoomDetails, empty_rooms: Option<bool>) -> bool {
	empty_rooms.is_none_or(|want| (room.joined_local_members == uint!(0)) == want)
}

/// Orders rooms ascending by the requested column, tiebreaking on room id for
/// deterministic pagination. The deprecated `Alphabetical` and `Size` aliases
/// resolve to their current columns.
fn sort_rooms(rooms: &mut [RoomDetails], order_by: &RoomSortOrder) {
	let tiebreak = |a: &RoomDetails, b: &RoomDetails, primary: Ordering| {
		primary.then_with(|| a.room_id.cmp(&b.room_id))
	};

	match order_by {
		| RoomSortOrder::CanonicalAlias =>
			rooms.sort_by(|a, b| tiebreak(a, b, a.canonical_alias.cmp(&b.canonical_alias))),
		| RoomSortOrder::JoinedMembers | RoomSortOrder::Size =>
			rooms.sort_by(|a, b| tiebreak(a, b, a.joined_members.cmp(&b.joined_members))),
		| RoomSortOrder::JoinedLocalMembers => rooms.sort_by(|a, b| {
			tiebreak(
				a,
				b,
				a.joined_local_members
					.cmp(&b.joined_local_members),
			)
		}),
		| RoomSortOrder::Version =>
			rooms.sort_by(|a, b| tiebreak(a, b, a.version.cmp(&b.version))),
		| RoomSortOrder::Creator =>
			rooms.sort_by(|a, b| tiebreak(a, b, a.creator.cmp(&b.creator))),
		| RoomSortOrder::Encryption =>
			rooms.sort_by(|a, b| tiebreak(a, b, a.encryption.cmp(&b.encryption))),
		| RoomSortOrder::Federatable =>
			rooms.sort_by(|a, b| tiebreak(a, b, a.federatable.cmp(&b.federatable))),
		| RoomSortOrder::Public => rooms.sort_by(|a, b| tiebreak(a, b, a.public.cmp(&b.public))),
		| RoomSortOrder::JoinRules => rooms.sort_by(|a, b| {
			tiebreak(a, b, enum_key(a.join_rules.as_ref()).cmp(&enum_key(b.join_rules.as_ref())))
		}),
		| RoomSortOrder::GuestAccess => rooms.sort_by(|a, b| {
			tiebreak(
				a,
				b,
				enum_key(a.guest_access.as_ref()).cmp(&enum_key(b.guest_access.as_ref())),
			)
		}),
		| RoomSortOrder::HistoryVisibility => rooms.sort_by(|a, b| {
			tiebreak(
				a,
				b,
				enum_key(a.history_visibility.as_ref())
					.cmp(&enum_key(b.history_visibility.as_ref())),
			)
		}),
		| RoomSortOrder::StateEvents =>
			rooms.sort_by(|a, b| tiebreak(a, b, a.state_events.cmp(&b.state_events))),
		| _ => rooms.sort_by(|a, b| tiebreak(a, b, a.name.cmp(&b.name))),
	}
}

/// Projects an optional string enum to a comparable key.
fn enum_key<T: AsRef<str>>(value: Option<&T>) -> Option<&str> { value.map(AsRef::as_ref) }

#[cfg(test)]
mod tests {
	use ruma::{room_alias_id, room_id};

	use super::{RoomDetails, matches_search};

	fn room() -> RoomDetails { RoomDetails::new(room_id!("!abcdef:example.org").to_owned()) }

	#[test]
	fn search_none_matches_every_room() {
		assert!(matches_search(&room(), None));
	}

	#[test]
	fn search_matches_name_substring_case_insensitively() {
		let mut room = room();
		room.name = Some("The Lounge".to_owned());

		assert!(matches_search(&room, Some("lounge")));
		assert!(!matches_search(&room, Some("kitchen")));
	}

	#[test]
	fn search_matches_canonical_alias_substring() {
		let mut room = room();
		room.canonical_alias = Some(room_alias_id!("#lounge:example.org").to_owned());

		assert!(matches_search(&room, Some("loung")));
	}

	#[test]
	fn search_matches_room_id_only_by_exact_equality() {
		let room = room();

		assert!(matches_search(&room, Some("!abcdef:example.org")));
		// A substring of the room id must not match: the id is compared exactly.
		assert!(!matches_search(&room, Some("abcdef")));
	}
}
