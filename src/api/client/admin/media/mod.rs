//! Synapse admin API: media endpoints.

mod delete_media;
mod delete_media_by_date_size;
mod delete_user_media;
mod list_user_media;

use ruma::{
	UInt,
	api::Direction::{self, Backward, Forward},
};
use synapse_admin_api::media::list_user_media::v1::MediaSortOrder;
use tuwunel_service::media::UserMediaEntry;

pub(crate) use self::{
	delete_media::admin_delete_media_route,
	delete_media_by_date_size::admin_delete_media_by_date_size_route,
	delete_user_media::admin_delete_user_media_route,
	list_user_media::admin_list_user_media_route,
};

/// Sort field selector, mapping the wire `order_by` onto the derivable columns.
#[derive(Clone, Copy)]
pub(super) enum Column {
	MediaId,
	UploadName,
	CreatedTs,
	MediaLength,
	MediaType,
}

/// Sort key for a media row under the requested `order_by` column.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortKey<'a> {
	Str(&'a str),
	OptStr(Option<&'a str>),
	Num(Option<u64>),
}

/// Selects one page of a user's media, applying the same sort, direction, and
/// windowing as Synapse's shared list/delete selection.
pub(super) fn select_page(
	entries: Vec<UserMediaEntry>,
	order_by: Option<&MediaSortOrder>,
	dir: Option<Direction>,
	from: usize,
	limit: usize,
) -> Vec<UserMediaEntry> {
	paginate(entries, column_of(order_by), direction_of(order_by, dir), from, limit)
}

fn column_of(order_by: Option<&MediaSortOrder>) -> Column {
	match order_by {
		| Some(MediaSortOrder::MediaId) => Column::MediaId,
		| Some(MediaSortOrder::UploadName) => Column::UploadName,
		| Some(MediaSortOrder::MediaLength) => Column::MediaLength,
		| Some(MediaSortOrder::MediaType) => Column::MediaType,
		| _ => Column::CreatedTs,
	}
}

/// Default direction is newest first (`Backward`) when the client sets neither
/// `order_by` nor `dir`, matching Synapse's back-compat behaviour.
fn direction_of(order_by: Option<&MediaSortOrder>, dir: Option<Direction>) -> Direction {
	match (order_by, dir) {
		| (None, None) => Backward,
		| (_, Some(dir)) => dir,
		| (Some(_), None) => Forward,
	}
}

fn paginate(
	mut entries: Vec<UserMediaEntry>,
	column: Column,
	dir: Direction,
	from: usize,
	limit: usize,
) -> Vec<UserMediaEntry> {
	entries.sort_by(|a, b| {
		sort_key(column, a)
			.cmp(&sort_key(column, b))
			.then_with(|| a.mxc.cmp(&b.mxc))
	});

	if matches!(dir, Backward) {
		entries.reverse();
	}

	entries
		.into_iter()
		.skip(from)
		.take(limit)
		.collect()
}

fn sort_key(column: Column, entry: &UserMediaEntry) -> SortKey<'_> {
	match column {
		| Column::MediaId => SortKey::Str(entry.mxc.media_id().unwrap_or_default()),
		| Column::UploadName => SortKey::OptStr(entry.upload_name.as_deref()),
		| Column::MediaType => SortKey::OptStr(entry.media_type.as_deref()),
		| Column::MediaLength => SortKey::Num(entry.media_length),
		| Column::CreatedTs => SortKey::Num(Some(entry.created_ts)),
	}
}

pub(super) fn usize_from(value: UInt) -> usize {
	usize::try_from(u64::from(value)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
	use ruma::{
		api::Direction::{Backward, Forward},
		user_id,
	};

	use super::{Column, UserMediaEntry, paginate};

	fn entry(media_id: &str, created_ts: u64, media_length: Option<u64>) -> UserMediaEntry {
		UserMediaEntry {
			mxc: format!("mxc://example.org/{media_id}").into(),
			media_type: Some("image/png".to_owned()),
			upload_name: Some(format!("{media_id}.png")),
			media_length,
			created_ts,
			user_id: user_id!("@alice:example.org").to_owned(),
		}
	}

	fn ids(entries: &[UserMediaEntry]) -> Vec<&str> {
		entries
			.iter()
			.map(|e| e.mxc.media_id().unwrap_or_default())
			.collect()
	}

	#[test]
	fn created_ts_backward_is_newest_first() {
		let entries = vec![entry("a", 10, None), entry("b", 30, None), entry("c", 20, None)];

		let page = paginate(entries, Column::CreatedTs, Backward, 0, 100);

		assert_eq!(ids(&page), ["b", "c", "a"]);
	}

	#[test]
	fn created_ts_forward_is_oldest_first() {
		let entries = vec![entry("a", 10, None), entry("b", 30, None), entry("c", 20, None)];

		let page = paginate(entries, Column::CreatedTs, Forward, 0, 100);

		assert_eq!(ids(&page), ["a", "c", "b"]);
	}

	#[test]
	fn from_and_limit_window_the_page() {
		let entries = vec![entry("a", 10, None), entry("b", 30, None), entry("c", 20, None)];

		let page = paginate(entries, Column::CreatedTs, Forward, 1, 1);

		assert_eq!(ids(&page), ["c"]);
	}

	#[test]
	fn media_length_backward_puts_largest_first() {
		let entries =
			vec![entry("a", 10, Some(5)), entry("b", 10, Some(30)), entry("c", 10, Some(20))];

		let page = paginate(entries, Column::MediaLength, Backward, 0, 100);

		assert_eq!(ids(&page), ["b", "c", "a"]);
	}
}
