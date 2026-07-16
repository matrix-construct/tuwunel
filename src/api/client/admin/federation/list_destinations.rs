use std::cmp::Ordering;

use axum::extract::State;
use futures::StreamExt;
use ruma::{ServerName, api::Direction};
use synapse_admin_api::federation::{
	Destination,
	list_destinations::v1::{DestinationSortOrder, Request, Response},
};
use tuwunel_core::{
	Err, Result,
	utils::{
		ReadyExt,
		math::{ruma_from_usize, usize_from_ruma},
	},
};

use super::destination_from_backoff;
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/federation/destinations`
pub(crate) async fn admin_list_destinations_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let default_order = DestinationSortOrder::Destination;
	let order_by = body.order_by.as_ref().unwrap_or(&default_order);

	if !matches!(
		order_by,
		DestinationSortOrder::Destination
			| DestinationSortOrder::RetryLastTs
			| DestinationSortOrder::RetryInterval
			| DestinationSortOrder::FailureTs
			| DestinationSortOrder::LastSuccessfulStreamOrdering
	) {
		return Err!(Request(InvalidParam("Unknown order_by parameter")));
	}

	let dir = body.dir.unwrap_or(Direction::Forward);
	let from = body.from.map_or(0, usize_from_ruma);
	let limit = body.limit.map_or(100, usize_from_ruma);

	let needle = body
		.destination
		.as_deref()
		.filter(|needle| !needle.is_empty());

	let mut backoffs = services.federation.peer_backoffs().await;

	let own_server = services.globals.server_name();

	// Self is never a federation destination; drop any stray self-row.
	backoffs.remove(own_server);

	let mut rows: Vec<Destination> = services
		.state_cache
		.servers()
		.ready_filter(|server| *server != own_server && name_matches(server, needle))
		.map(|server| destination_from_backoff(server.to_owned(), backoffs.remove(server)))
		.collect()
		.await;

	rows.extend(
		backoffs
			.into_iter()
			.filter(|(server, _)| name_matches(server, needle))
			.map(|(server, backoff)| destination_from_backoff(server, Some(backoff))),
	);

	rows.sort_unstable_by(|a, b| {
		let ordering = destination_ordering(order_by, a, b);

		let ordering = match dir {
			| Direction::Forward => ordering,
			| Direction::Backward => ordering.reverse(),
		};

		ordering.then_with(|| a.destination.cmp(&b.destination))
	});

	let matched_count = rows.len();
	let total = ruma_from_usize(matched_count);

	let destinations: Vec<Destination> = rows.into_iter().skip(from).take(limit).collect();

	let end = from.saturating_add(destinations.len());
	let next_token = (end < matched_count).then(|| end.to_string());

	Ok(Response { destinations, total, next_token })
}

/// Case-insensitive substring match of the server name, mirroring Synapse's
/// `LIKE %needle%`. A `None` or empty needle matches every server.
fn name_matches(server: &ServerName, needle: Option<&str>) -> bool {
	let Some(needle) = needle.filter(|needle| !needle.is_empty()) else {
		return true;
	};

	server
		.as_bytes()
		.windows(needle.len())
		.any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// The `Destination` and the doc-hidden `_Custom` variant both fall through to
/// the server-name tiebreak.
fn destination_ordering(
	order_by: &DestinationSortOrder,
	a: &Destination,
	b: &Destination,
) -> Ordering {
	match order_by {
		| DestinationSortOrder::RetryLastTs => a.retry_last_ts.cmp(&b.retry_last_ts),
		| DestinationSortOrder::RetryInterval => a.retry_interval.cmp(&b.retry_interval),
		| DestinationSortOrder::FailureTs => a.failure_ts.cmp(&b.failure_ts),
		| DestinationSortOrder::LastSuccessfulStreamOrdering => a
			.last_successful_stream_ordering
			.cmp(&b.last_successful_stream_ordering),
		| _ => a.destination.cmp(&b.destination),
	}
}

#[cfg(test)]
mod tests {
	use std::cmp::Ordering;

	use ruma::{UInt, server_name, uint};
	use tuwunel_service::federation::PeerBackoff;

	use super::{
		Destination, DestinationSortOrder, destination_from_backoff, destination_ordering,
		name_matches,
	};

	#[test]
	fn healthy_destination_is_all_zeros() {
		let row = destination_from_backoff(server_name!("matrix.org").to_owned(), None);

		assert_eq!(row.retry_last_ts, uint!(0));
		assert_eq!(row.retry_interval, uint!(0));
		assert_eq!(row.failure_ts, None);
		assert_eq!(row.last_successful_stream_ordering, None);
	}

	#[test]
	fn failing_destination_maps_seconds_to_millis() {
		let backoff = PeerBackoff {
			anchor_secs: 10,
			oldest_secs: 5,
			delay_secs: 60,
		};

		let row = destination_from_backoff(server_name!("matrix.org").to_owned(), Some(backoff));

		assert_eq!(row.retry_last_ts, uint!(10_000));
		assert_eq!(row.retry_interval, uint!(60_000));
		assert_eq!(row.failure_ts, Some(uint!(5_000)));
		assert_eq!(row.last_successful_stream_ordering, None);
	}

	#[test]
	fn millis_saturates() {
		let backoff = PeerBackoff {
			anchor_secs: u64::MAX,
			oldest_secs: 0,
			delay_secs: 0,
		};

		let row = destination_from_backoff(server_name!("matrix.org").to_owned(), Some(backoff));

		assert_eq!(row.retry_last_ts, UInt::MAX);
	}

	#[test]
	fn name_matches_is_ascii_case_insensitive() {
		let server = server_name!("matrix.org");

		assert!(name_matches(server, Some("MATRIX")));
		assert!(name_matches(server, Some("matrix")));
		assert!(!name_matches(server_name!("example.com"), Some("matrix")));
		assert!(name_matches(server, None));
		assert!(name_matches(server, Some("")));
		assert!(!name_matches(server, Some("matrix.organization")));
	}

	#[test]
	fn ordering_selects_the_field() {
		let low = Destination {
			retry_last_ts: uint!(1),
			retry_interval: uint!(9),
			failure_ts: None,
			..Destination::new(server_name!("a.example").to_owned())
		};

		let high = Destination {
			retry_last_ts: uint!(2),
			retry_interval: uint!(8),
			failure_ts: Some(uint!(1)),
			last_successful_stream_ordering: Some(uint!(1)),
			..Destination::new(server_name!("b.example").to_owned())
		};

		assert_eq!(
			destination_ordering(&DestinationSortOrder::RetryLastTs, &low, &high),
			Ordering::Less
		);
		assert_eq!(
			destination_ordering(&DestinationSortOrder::RetryInterval, &low, &high),
			Ordering::Greater
		);
		assert_eq!(
			destination_ordering(&DestinationSortOrder::FailureTs, &low, &high),
			Ordering::Less
		);
		assert_eq!(
			destination_ordering(
				&DestinationSortOrder::LastSuccessfulStreamOrdering,
				&low,
				&high,
			),
			Ordering::Less
		);
		assert_eq!(
			destination_ordering(&DestinationSortOrder::Destination, &low, &high),
			Ordering::Less
		);
	}
}
