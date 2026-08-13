use std::{
	collections::{BTreeSet, VecDeque},
};

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
	utils::stream::{TryWidebandExt, automatic_width},
};

use super::AccessCheck;
use crate::Ruma;

/// arbitrary number but synapse's is 20 and we can handle lots of these anyways
const LIMIT_MAX: usize = 50;

/// spec says default is 10
const LIMIT_DEFAULT: usize = 10;

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
	let limit = body
		.limit
		.try_into()
		.unwrap_or(LIMIT_DEFAULT)
		.min(LIMIT_MAX);

	let mut seen = body
		.earliest_events
		.iter()
		.cloned()
		.collect::<BTreeSet<_>>();

	let pending = body
		.latest_events
		.iter()
		.filter(|event_id| seen.insert((*event_id).clone()))
		.cloned()
		.map(|event_id| (event_id, true))
		.collect::<VecDeque<_>>();

	let fetches = FuturesOrdered::new();

	let events =
		unfold((seen, pending, fetches), async |(mut seen, mut pending, mut fetches)| {
			let event =
				next_missing_event(&body, &fetch, &mut seen, &mut pending, &mut fetches).await?;

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

async fn next_missing_event<Fetch, Fut>(
	body: &Ruma<get_missing_events::v1::Request>,
	fetch: &Fetch,
	seen: &mut BTreeSet<OwnedEventId>,
	pending: &mut VecDeque<(OwnedEventId, bool)>,
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
			.filter(|event_id| seen.insert(event_id.clone()))
			.for_each(|event_id| pending.push_back((event_id, false)));

		if !is_latest {
			return Some((event_id, event));
		}
	}
}
