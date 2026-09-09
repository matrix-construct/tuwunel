use std::collections::{BTreeSet, VecDeque};

use axum::extract::State;
use futures::{
	StreamExt, TryFutureExt, TryStreamExt,
	future::try_join,
	stream::{FuturesOrdered, unfold},
};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId,
	api::federation::event::get_missing_events, canonical_json::redact_in_place,
};
use tuwunel_core::{
	Error, Result, debug, err,
	matrix::room_version::rules as room_version_rules,
	utils::{
		math::usize_from_ruma_bounded,
		stream::{TryWidebandExt, automatic_width},
	},
};

use super::AccessCheck;
use crate::Ruma;

type Seen = BTreeSet<OwnedEventId>;
type Pending = VecDeque<(OwnedEventId, bool)>;

/// arbitrary number but synapse's is 20 and we can handle lots of these anyways
const LIMIT_MAX: usize = 50;

/// spec says default is 10
const LIMIT_DEFAULT: usize = 10;

// Caps events walked: seeds taken from latest_events, then again past them.
const WALK_MAX: usize = 256;

// Caps earliest_events, which only prune the walk and cost no lookups.
const EARLIEST_MAX: usize = 4096;

/// # `POST /_matrix/federation/v1/get_missing_events/{roomId}`
///
/// Retrieves events that the sender is missing.
pub(crate) async fn get_missing_events_route(
	State(services): State<crate::State>,
	body: Ruma<get_missing_events::v1::Request>,
) -> Result<get_missing_events::v1::Response> {
	let access_check = AccessCheck {
		services: &services,
		origin: body.origin(),
		room_id: &body.room_id,
		event_id: None,
	};

	let room_version = services.state.get_room_version(&body.room_id);

	let (room_version, ()) = try_join(room_version, access_check.check()).await?;

	let rules = room_version_rules(&room_version)?;

	let fetch = async |(event_id, is_latest): (OwnedEventId, bool)| {
		let event = services.timeline.get_pdu_json(&event_id).await;

		(event_id, is_latest, event)
	};

	// min_depth is intentionally ignored, matching Synapse's responder.
	let limit = usize_from_ruma_bounded(body.limit, LIMIT_DEFAULT, LIMIT_MAX);

	let (seen, pending) = walk_seed(&body);
	let seen_max = seen.len().saturating_add(WALK_MAX);

	let fetches = FuturesOrdered::new();

	let events =
		unfold((seen, pending, fetches), async |(mut seen, mut pending, mut fetches)| {
			let event = next_missing_event(
				&body,
				&fetch,
				seen_max,
				&mut seen,
				&mut pending,
				&mut fetches,
			)
			.await?;

			Some((event, (seen, pending, fetches)))
		})
		.take(limit)
		.map(Ok::<_, Error>)
		.wide_and_then(async |(event_id, mut event)| {
			let visible = services
				.state_accessor
				.server_can_see_event(body.origin(), &body.room_id, &event_id)
				.await;

			let event = if visible {
				services
					.state_accessor
					.erased_for_server(body.origin(), event)
					.await
			} else {
				redact_in_place(&mut event, &rules.redaction, None)
					.map_err(|error| err!(Database("Failed to redact event: {error}")))?;

				event
			};

			let event = services
				.federation
				.format_pdu_into(event, Some(&room_version))
				.await;

			Ok(event)
		})
		.try_collect::<Vec<_>>()
		.map_ok(|mut vec| {
			vec.reverse();
			vec
		})
		.await?;

	Ok(get_missing_events::v1::Response { events })
}

/// Builds the walk's dedup set and its initial queue.
///
/// Both caller-supplied vectors are bounded here so the traversal cost stays
/// independent of the request size. The flag marks a seed entry, which enqueues
/// its prev_events without yielding itself.
fn walk_seed(body: &get_missing_events::v1::Request) -> (Seen, Pending) {
	let mut seen: Seen = body
		.earliest_events
		.iter()
		.take(EARLIEST_MAX)
		.cloned()
		.collect();

	let pending = body
		.latest_events
		.iter()
		.take(WALK_MAX)
		.filter(|event_id| seen.insert((*event_id).clone()))
		.cloned()
		.map(|event_id| (event_id, true))
		.collect();

	(seen, pending)
}

async fn next_missing_event<Fetch, Fut>(
	body: &Ruma<get_missing_events::v1::Request>,
	fetch: &Fetch,
	seen_max: usize,
	seen: &mut Seen,
	pending: &mut Pending,
	fetches: &mut FuturesOrdered<Fut>,
) -> Option<(OwnedEventId, CanonicalJsonObject)>
where
	Fetch: Fn((OwnedEventId, bool)) -> Fut + Sync,
	Fut: Future<Output = (OwnedEventId, bool, Result<CanonicalJsonObject>)> + Send,
{
	loop {
		let width = automatic_width();

		while fetches.len() < width
			&& let Some(input) = pending.pop_front()
		{
			fetches.push_back(fetch(input));
		}

		let (event_id, is_latest, event) = fetches.next().await?;
		let Ok(event) = event else {
			debug!(
				?body.origin,
				%event_id,
				"Event does not exist locally, skipping"
			);

			continue;
		};

		if event
			.get("room_id")
			.and_then(CanonicalJsonValue::as_str)
			!= Some(body.room_id.as_str())
		{
			continue;
		}

		event
			.get("prev_events")
			.and_then(CanonicalJsonValue::as_array)
			.into_iter()
			.flatten()
			.filter_map(CanonicalJsonValue::as_str)
			.filter_map(|event_id| EventId::parse(event_id).ok())
			.filter(|event_id| seen.len() < seen_max && seen.insert(event_id.clone()))
			.for_each(|event_id| pending.push_back((event_id, false)));

		if !is_latest {
			return Some((event_id, event));
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

	use axum_extra::extract::cookie::CookieJar;
	use futures::stream::FuturesOrdered;
	use ruma::{
		CanonicalJsonObject, EventId, OwnedEventId, RoomId,
		api::federation::event::get_missing_events::v1::Request, room_id, server_name,
	};
	use serde_json::json;
	use tuwunel_core::matrix::pdu::MAX_PREV_EVENTS;

	use super::{EARLIEST_MAX, Ruma, WALK_MAX, err, next_missing_event, walk_seed};

	fn event_ids(prefix: &str, len: usize) -> Vec<OwnedEventId> {
		(0..len)
			.map(|index| {
				EventId::parse(format!("${prefix}{index}:example.com")).expect("valid event id")
			})
			.collect()
	}

	fn request(earliest: Vec<OwnedEventId>, latest: Vec<OwnedEventId>) -> Ruma<Request> {
		let body = Request::new(room_id!("!room:example.com").to_owned(), earliest, latest);

		Ruma {
			body,
			cookie: CookieJar::new(),
			origin: Some(server_name!("example.com").to_owned()),
			sender_user: None,
			sender_device: None,
			appservice_info: None,
			json_body: None,
		}
	}

	fn event_with_prevs(room_id: &RoomId, index: usize) -> CanonicalJsonObject {
		let prev_events: Vec<_> = (0..MAX_PREV_EVENTS)
			.map(|prev| format!("$p{index}a{prev}:example.com"))
			.collect();
		let value = json!({
			"room_id": room_id,
			"prev_events": prev_events,
		});

		serde_json::from_value(value).expect("valid canonical json")
	}

	#[test]
	fn seed_bounded_by_cap_not_request() {
		let body = request(Vec::new(), event_ids("latest", 5_000));
		let (seen, pending) = walk_seed(&body);

		assert_eq!(seen.len(), WALK_MAX);
		assert_eq!(pending.len(), WALK_MAX);

		let body = request(event_ids("earliest", 10_000), event_ids("latest", 5_000));
		let (seen, pending) = walk_seed(&body);

		assert_eq!(seen.len(), EARLIEST_MAX + WALK_MAX);
		assert_eq!(pending.len(), WALK_MAX);
	}

	#[tokio::test]
	async fn walk_lookups_bounded_by_cap_not_request() {
		let body = request(Vec::new(), event_ids("latest", 5_000));
		let (mut seen, mut pending) = walk_seed(&body);
		let seen_max = seen.len().saturating_add(WALK_MAX);
		let lookups = AtomicUsize::new(0);
		let room_id = body.room_id.clone();
		let fetch = async |(event_id, is_latest): (OwnedEventId, bool)| {
			let index = lookups.fetch_add(1, Relaxed);
			let event = is_latest
				.then(|| event_with_prevs(&room_id, index))
				.ok_or_else(|| err!(Request(NotFound("Event not found."))));

			(event_id, is_latest, event)
		};

		let mut fetches = FuturesOrdered::new();

		let result =
			next_missing_event(&body, &fetch, seen_max, &mut seen, &mut pending, &mut fetches)
				.await;

		assert!(result.is_none());
		assert_eq!(lookups.load(Relaxed), 2 * WALK_MAX);
	}
}
