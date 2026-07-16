use std::collections::BTreeMap;

use axum::extract::State;
use futures::StreamExt;
use ruma::{
	OwnedUserId, UInt,
	api::Direction::{self, Backward, Forward},
};
use synapse_admin_api::statistics::user_media_statistics::v1::{
	Request, Response, UserMediaSortOrder, UserMediaStat,
};
use tuwunel_core::{
	Err, Result,
	utils::{IterStream, ReadyExt, math::ruma_from_usize, stream::BroadbandExt},
};

use super::{SortKey, usize_from};
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/statistics/users/media`
///
/// Media creation times derive from storage-object modification time, so the
/// `from_ts`/`until_ts` window selects by mtime and media missing from every
/// storage provider are not counted.
pub(crate) async fn admin_user_media_statistics_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let order_by = body
		.order_by
		.as_ref()
		.unwrap_or(&UserMediaSortOrder::UserId);

	if !matches!(
		order_by,
		UserMediaSortOrder::MediaLength
			| UserMediaSortOrder::MediaCount
			| UserMediaSortOrder::UserId
			| UserMediaSortOrder::Displayname
	) {
		return Err!(Request(InvalidParam(
			"Query parameter order_by must be one of media_length, media_count, user_id, \
			 displayname."
		)));
	}

	let from_ts = body.from_ts.map_or(0, u64::from);
	let until_ts = body.until_ts.map(u64::from);
	if until_ts.is_some_and(|until_ts| until_ts <= from_ts) {
		return Err!(Request(InvalidParam(
			"Query parameter until_ts must be greater than from_ts."
		)));
	}

	let search_term = body.search_term.as_deref();
	if search_term.is_some_and(str::is_empty) {
		return Err!(Request(InvalidParam(
			"Query parameter search_term cannot be an empty string."
		)));
	}

	let usage = services
		.media
		.upload_stats()
		.ready_filter(|stat| in_window(stat.created_ts, from_ts, until_ts))
		.ready_fold(BTreeMap::new(), |mut usage: BTreeMap<OwnedUserId, (u64, u64)>, stat| {
			let (count, length) = usage.entry(stat.user_id).or_default();
			*count = count.saturating_add(1);
			*length = length.saturating_add(stat.media_length);
			usage
		})
		.await;

	let rows: Vec<UserMediaStat> = usage
		.into_iter()
		.stream()
		.broad_then(async |(user_id, (count, length))| {
			let displayname = services.profile.displayname(&user_id).await.ok();

			UserMediaStat {
				displayname,
				..UserMediaStat::new(user_id, uint_from_u64(count), uint_from_u64(length))
			}
		})
		.ready_filter(|row| search_term.is_none_or(|term| row_matches(row, term)))
		.collect()
		.await;

	let from = body.from.map_or(0, usize_from);
	let limit = body.limit.map_or(100, usize_from);
	let dir = body.dir.unwrap_or(Forward);

	Ok(into_response(rows, order_by, dir, from, limit))
}

fn into_response(
	rows: Vec<UserMediaStat>,
	order_by: &UserMediaSortOrder,
	dir: Direction,
	from: usize,
	limit: usize,
) -> Response {
	let total = rows.len();
	let users = paginate(rows, order_by, dir, from, limit);
	let end = from.saturating_add(users.len());
	let next_token = (end < total).then(|| ruma_from_usize(end));

	Response {
		users,
		next_token,
		total: ruma_from_usize(total),
	}
}

fn uint_from_u64(value: u64) -> UInt { UInt::try_from(value).unwrap_or(UInt::MAX) }

fn in_window(created_ts: u64, from_ts: u64, until_ts: Option<u64>) -> bool {
	created_ts >= from_ts && until_ts.is_none_or(|until_ts| created_ts <= until_ts)
}

fn row_matches(row: &UserMediaStat, term: &str) -> bool {
	row.user_id.localpart().contains(term)
		|| row
			.displayname
			.as_deref()
			.is_some_and(|name| name.contains(term))
}

fn paginate(
	mut rows: Vec<UserMediaStat>,
	order_by: &UserMediaSortOrder,
	dir: Direction,
	from: usize,
	limit: usize,
) -> Vec<UserMediaStat> {
	rows.sort_unstable_by(|a, b| {
		sort_key(order_by, a)
			.cmp(&sort_key(order_by, b))
			.then_with(|| a.user_id.cmp(&b.user_id))
	});

	if matches!(dir, Backward) {
		rows.reverse();
	}

	rows.into_iter().skip(from).take(limit).collect()
}

fn sort_key<'a>(order_by: &UserMediaSortOrder, row: &'a UserMediaStat) -> SortKey<'a> {
	match order_by {
		| UserMediaSortOrder::MediaLength => SortKey::Num(Some(row.media_length.into())),
		| UserMediaSortOrder::MediaCount => SortKey::Num(Some(row.media_count.into())),
		| UserMediaSortOrder::Displayname => SortKey::OptStr(row.displayname.as_deref()),
		| _ => SortKey::Str(row.user_id.as_str()),
	}
}

#[cfg(test)]
mod tests {
	use ruma::{
		UInt,
		api::Direction::{Backward, Forward},
	};

	use super::{
		UserMediaSortOrder, UserMediaStat, in_window, into_response, paginate, row_matches,
		uint_from_u64,
	};

	fn row(local: &str, displayname: Option<&str>, count: u32, length: u32) -> UserMediaStat {
		let user_id = format!("@{local}:example.org")
			.try_into()
			.unwrap();

		UserMediaStat {
			displayname: displayname.map(ToOwned::to_owned),
			..UserMediaStat::new(user_id, count.into(), length.into())
		}
	}

	fn ids(rows: &[UserMediaStat]) -> Vec<&str> {
		rows.iter()
			.map(|row| row.user_id.localpart())
			.collect()
	}
	#[test]
	fn user_id_forward_orders_lexically() {
		let rows = vec![row("b", None, 1, 1), row("a", None, 1, 1), row("c", None, 1, 1)];

		let page = paginate(rows, &UserMediaSortOrder::UserId, Forward, 0, 100);

		assert_eq!(ids(&page), ["a", "b", "c"]);
	}

	#[test]
	fn backward_reverses_order() {
		let rows = vec![row("b", None, 1, 1), row("a", None, 1, 1), row("c", None, 1, 1)];

		let page = paginate(rows, &UserMediaSortOrder::UserId, Backward, 0, 100);

		assert_eq!(ids(&page), ["c", "b", "a"]);
	}

	#[test]
	fn media_length_orders_numerically() {
		let rows = vec![row("a", None, 1, 5), row("b", None, 1, 30), row("c", None, 1, 20)];

		let forward = paginate(rows.clone(), &UserMediaSortOrder::MediaLength, Forward, 0, 100);

		assert_eq!(ids(&forward), ["a", "c", "b"]);

		let backward = paginate(rows, &UserMediaSortOrder::MediaLength, Backward, 0, 100);

		assert_eq!(ids(&backward), ["b", "c", "a"]);
	}

	#[test]
	fn equal_counts_tiebreak_by_user_id() {
		let rows = vec![row("c", None, 5, 1), row("a", None, 5, 1), row("b", None, 5, 1)];

		let page = paginate(rows, &UserMediaSortOrder::MediaCount, Forward, 0, 100);

		assert_eq!(ids(&page), ["a", "b", "c"]);
	}

	#[test]
	fn displayname_none_sorts_first_forward() {
		let rows = vec![row("a", Some("zed"), 1, 1), row("b", None, 1, 1)];

		let page = paginate(rows, &UserMediaSortOrder::Displayname, Forward, 0, 100);

		assert_eq!(ids(&page), ["b", "a"]);
	}

	#[test]
	fn from_and_limit_window_the_page() {
		let rows = vec![row("a", None, 1, 1), row("b", None, 1, 1), row("c", None, 1, 1)];

		let page = paginate(rows, &UserMediaSortOrder::UserId, Forward, 1, 1);

		assert_eq!(ids(&page), ["b"]);
	}

	#[test]
	fn oversized_aggregate_saturates_on_wire() {
		assert_eq!(uint_from_u64(42), UInt::from(42_u32));
		assert_eq!(uint_from_u64(u64::MAX), UInt::MAX);
	}

	#[test]
	fn response_reports_total_and_next_token() {
		let rows = vec![row("a", None, 1, 1), row("b", None, 1, 1), row("c", None, 1, 1)];

		let response = into_response(rows, &UserMediaSortOrder::UserId, Forward, 0, 1);

		assert_eq!(ids(&response.users), ["a"]);
		assert_eq!(response.total, UInt::from(3_u32));
		assert_eq!(response.next_token, Some(UInt::from(1_u32)));
	}

	#[test]
	fn final_page_omits_next_token() {
		let rows = vec![row("a", None, 1, 1), row("b", None, 1, 1), row("c", None, 1, 1)];

		let response = into_response(rows, &UserMediaSortOrder::UserId, Forward, 1, 2);

		assert_eq!(ids(&response.users), ["b", "c"]);
		assert_eq!(response.total, UInt::from(3_u32));
		assert_eq!(response.next_token, None);
	}

	#[test]
	fn zero_limit_repeats_from_as_next_token() {
		let rows = vec![row("a", None, 1, 1), row("b", None, 1, 1), row("c", None, 1, 1)];

		let response = into_response(rows, &UserMediaSortOrder::UserId, Forward, 1, 0);

		assert!(response.users.is_empty());
		assert_eq!(response.total, UInt::from(3_u32));
		assert_eq!(response.next_token, Some(UInt::from(1_u32)));
	}

	#[test]
	fn window_bounds_are_inclusive() {
		assert!(in_window(5, 5, Some(5)));
		assert!(!in_window(4, 5, None));
		assert!(!in_window(6, 0, Some(5)));
		assert!(in_window(0, 0, None));
	}

	#[test]
	fn search_matches_localpart_or_displayname() {
		assert!(row_matches(&row("alice", None, 1, 1), "lic"));
		assert!(row_matches(&row("bob", Some("Wonderland"), 1, 1), "onder"));
		assert!(!row_matches(&row("alice", Some("alice"), 1, 1), "example"));
		assert!(!row_matches(&row("alice", Some("alice"), 1, 1), "Alice"));
	}
}
