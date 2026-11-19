//! Admission checks for sliding sync's sticky list and extension configuration.

use std::{borrow::Cow, collections::HashSet};

use ruma::api::client::sync::sync_events::v5::request::{
	ExtensionRoomConfig, Extensions, ListFilters,
};
use tuwunel_core::{Err, Result};
use tuwunel_service::sync::distinct_filters;

use super::{
	Connection, Request,
	selection::{Bits, Numbered},
};

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
	check_with(request, cached, true)
}

/// Check the wire request alone. For a resume this skips the combined budget,
/// which depends on the sticky filters its connection inherits.
pub(super) fn check_wire(request: &Request, resume: bool) -> Result {
	check_with(request, None, !resume)
}

fn check_with(request: &Request, cached: Option<&Connection>, combined: bool) -> Result {
	let updated = request
		.lists
		.iter()
		.map(|(id, list)| (id, list, cached.and_then(|conn| conn.lists.get(id))));
	let retained = cached
		.into_iter()
		.flat_map(|conn| &conn.lists)
		.filter(|(id, _)| !request.lists.contains_key(*id))
		.map(|(id, list)| (id, list, None));

	let mut numbered = Numbered::default();
	let mut selections = Vec::new();
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
		if combined {
			// Group comparisons must use bounded vectors even when the wire contains repeats.
			let filters = filters.map(|f| {
				if [f.tags.len(), f.not_tags.len(), f.room_types.len(), f.not_room_types.len()]
					.into_iter()
					.any(|len| len > FILTER_ENTRIES_MAX)
				{
					Cow::Owned(distinct_filters(f))
				} else {
					Cow::Borrowed(f)
				}
			});
			selections.push((filters, numbered.add(keys)));
		}
	}

	let groups = co_applicable(&selections);
	for group in &groups {
		bounded(
			"distinct required_state of lists one room can match",
			group.len(),
			REQUIRED_STATE_MAX,
		)?;
	}

	// A room can enter every list, but it can have only one explicit subscription.
	// Do not charge different rooms' subscription selectors against one another.
	// Only list selectors are numbered; a subscription's are counted against
	// each group in their own scratch set of at most 256.
	let largest = groups.iter().map(Bits::len).max().unwrap_or(0);
	for sub in request.room_subscriptions.values() {
		let keys = &sub.required_state;
		bounded("required_state per subscription", keys.len(), REQUIRED_STATE_MAX)?;
		if largest.saturating_add(keys.len()) <= REQUIRED_STATE_MAX {
			continue;
		}
		let keys: HashSet<_> = keys.iter().collect();

		for group in &groups {
			let extra = keys
				.iter()
				.filter(|key| !numbered.contains(group, key))
				.count();

			bounded(
				"distinct list and subscription required_state",
				group.len().saturating_add(extra),
				REQUIRED_STATE_MAX,
			)?;
		}
	}

	check_scopes(&request.extensions, cached.map(|conn| &conn.extensions))
}

/// The selections of each group of lists that one room might match together.
///
/// A room has one value of `is_dm`, `is_encrypted` and `is_invite`, so only
/// lists admitting the same of the eight combinations can share it. Among
/// those, two lists cannot share a room when one requires a tag or room type
/// the other excludes or does not admit. A list's group is itself and every
/// list of its combination it can share a room with. Two members of a group
/// may still exclude each other, so a group can overcount a room's selectors.
fn co_applicable(lists: &[(Option<Cow<'_, ListFilters>>, Bits)]) -> HashSet<Bits> {
	let mut groups = HashSet::new();
	for flags in 0..8_u8 {
		let admits =
			|want: Option<bool>, flag: u8| want.is_none_or(|want| want == (flags & flag != 0));
		let members: Vec<_> = lists
			.iter()
			.filter(|(filters, _)| {
				filters.as_deref().is_none_or(|filters| {
					admits(filters.is_dm, 1)
						&& admits(filters.is_encrypted, 2)
						&& admits(filters.is_invite, 4)
				})
			})
			.collect();

		groups.extend(members.iter().map(|(filters, bits)| {
			members
				.iter()
				.filter(|(other, _)| !exclusive(filters.as_deref(), other.as_deref()))
				.fold(bits.clone(), |mut group, (_, other)| {
					group.union(other);
					group
				})
		}));
	}

	groups
}

/// Whether no room can match both filters, by its tags or its room type.
fn exclusive(a: Option<&ListFilters>, b: Option<&ListFilters>) -> bool {
	let excludes = |a: &ListFilters, b: &ListFilters| {
		let tags = !a.tags.is_empty() && a.tags.iter().all(|tag| b.not_tags.contains(tag));
		let room_types = !a.room_types.is_empty()
			&& a.room_types.iter().all(|room_type| {
				b.not_room_types.contains(room_type)
					|| (!b.room_types.is_empty() && !b.room_types.contains(room_type))
			});

		tags || room_types
	};

	a.zip(b)
		.is_some_and(|(a, b)| excludes(a, b) || excludes(b, a))
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
