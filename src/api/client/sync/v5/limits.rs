//! Admission checks for sliding sync's sticky list and extension configuration.

use std::collections::HashSet;

use ruma::api::client::sync::sync_events::v5::request::{ExtensionRoomConfig, Extensions};
use tuwunel_core::{Err, Result};

use super::{Connection, Request};

#[cfg(test)]
mod tests;

const LISTS_MAX: usize = 64;
const LIST_ID_BYTES_MAX: usize = 64;
const RANGES_MAX: usize = 16;
const FILTER_ENTRIES_MAX: usize = 16;
const REQUIRED_STATE_MAX: usize = 256;
const EXTENSION_ROOMS_MAX: usize = 256;

/// Check the configuration the request would use, without modifying the connection.
/// With no cached connection this checks just the wire request.
pub(super) fn check(request: &Request, cached: Option<&Connection>) -> Result {
	let updated = request
		.lists
		.iter()
		.map(|(id, list)| (id, list, cached.and_then(|conn| conn.lists.get(id))));
	let retained = cached
		.into_iter()
		.flat_map(|conn| &conn.lists)
		.filter(|(id, _)| !request.lists.contains_key(*id))
		.map(|(id, list)| (id, list, None));

	let mut list_state = HashSet::<&_>::new();
	for (i, (id, list, previous)) in updated.chain(retained).enumerate() {
		bounded("list count", i.saturating_add(1), LISTS_MAX)?;
		bounded("list ID length in bytes", id.len(), LIST_ID_BYTES_MAX)?;
		bounded_distinct("ranges per list", &list.ranges, RANGES_MAX)?;
		let filters = list
			.filters
			.as_ref()
			.or_else(|| previous?.filters.as_ref());

		if let Some(filters) = filters {
			bounded_distinct("spaces", &filters.spaces, FILTER_ENTRIES_MAX)?;
			bounded_distinct("tags", &filters.tags, FILTER_ENTRIES_MAX)?;
			bounded_distinct("not_tags", &filters.not_tags, FILTER_ENTRIES_MAX)?;
			bounded_distinct("room_types", &filters.room_types, FILTER_ENTRIES_MAX)?;
			bounded_distinct("not_room_types", &filters.not_room_types, FILTER_ENTRIES_MAX)?;
		}

		// An empty required_state on a list update keeps its previous selection.
		let keys = &list.room_details.required_state;
		let keys = previous
			.filter(|_| keys.is_empty())
			.map_or(keys, |old| &old.room_details.required_state);
		bounded("required_state per list", keys.len(), REQUIRED_STATE_MAX)?;
		list_state.extend(keys);
		bounded("distinct required_state across lists", list_state.len(), REQUIRED_STATE_MAX)?;
	}

	// A room can enter every list, but it can have only one explicit subscription.
	// Do not charge different rooms' subscription selectors against one another.
	let remaining = REQUIRED_STATE_MAX.saturating_sub(list_state.len());
	for sub in request.room_subscriptions.values() {
		let keys = &sub.required_state;
		bounded("required_state per subscription", keys.len(), REQUIRED_STATE_MAX)?;
		if keys.len() > remaining {
			let extra: HashSet<_> = keys
				.iter()
				.filter(|key| !list_state.contains(*key))
				.collect();
			bounded(
				"distinct list and subscription required_state",
				list_state.len().saturating_add(extra.len()),
				REQUIRED_STATE_MAX,
			)?;
		}
	}

	check_scopes(&request.extensions, cached.map(|conn| &conn.extensions))
}

fn check_scopes(request: &Extensions, cached: Option<&Extensions>) -> Result {
	// Ruma gives each extension its own type. In all five an absent scope
	// inherits, while a present (even empty) scope replaces the old one.
	macro_rules! scope {
		($extension:ident) => {{
			let new = &request.$extension;
			let old = cached.map(|ext| &ext.$extension);
			check_scope(
				new.lists
					.as_deref()
					.or_else(|| old?.lists.as_deref()),
				new.rooms
					.as_deref()
					.or_else(|| old?.rooms.as_deref()),
			)
		}};
	}
	scope!(account_data)?;
	scope!(receipts)?;
	scope!(typing)?;
	scope!(profiles)?;
	scope!(sticky_events)
}

fn check_scope<T: AsRef<str>>(
	lists: Option<&[T]>,
	rooms: Option<&[ExtensionRoomConfig]>,
) -> Result {
	if let Some(lists) = lists {
		bounded("lists per extension", lists.len(), LISTS_MAX)?;
		for id in lists {
			bounded("extension list ID length in bytes", id.as_ref().len(), LIST_ID_BYTES_MAX)?;
		}
	}
	bounded("rooms per extension", rooms.map_or(0, <[_]>::len), EXTENSION_ROOMS_MAX)
}

/// Refuses more than `max` distinct entries, stopping at the first distinct entry past it.
fn bounded_distinct<T: PartialEq>(field: &str, entries: &[T], max: usize) -> Result {
	let mut seen = Vec::new();
	for entry in entries {
		if !seen.contains(&entry) {
			seen.push(entry);
			bounded(field, seen.len(), max)?;
		}
	}

	Ok(())
}

fn bounded(field: &str, count: usize, max: usize) -> Result {
	if count > max {
		return Err!(Request(InvalidParam("Sliding sync {field} exceeds the limit of {max}.")));
	}
	Ok(())
}
