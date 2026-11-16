use std::{
	cmp::max,
	collections::{BTreeSet, VecDeque},
	iter::once,
	pin::pin,
};

use axum::extract::State;
use futures::{FutureExt, Stream, StreamExt, TryStreamExt, stream::unfold};
use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, ServerName,
	api::federation::backfill::get_backfill,
};
use tuwunel_core::{
	Result, at,
	utils::{
		BoolExt, IterStream, ReadyExt, TryReadyExt,
		math::usize_from_ruma_bounded,
		stream::{BroadbandExt, WidebandExt},
	},
};
use tuwunel_matrix::{PduCount, PduEvent};
use tuwunel_service::Services;

use super::AccessCheck;
use crate::Ruma;

type Candidate = Result<Option<CanonicalJsonObject>>;
type Pending = VecDeque<OwnedEventId>;
type Found = BTreeSet<OwnedEventId>;

/// arbitrary number but synapse's is 100 and we can handle lots of these
/// anyways
const LIMIT_MAX: usize = 150;
/// no spec defined number but we can handle a lot of these
const LIMIT_DEFAULT: usize = 50;

/// # `GET /_matrix/federation/v1/backfill/<room_id>`
///
/// Retrieves events from before the sender joined the room, if the room's
/// history visibility allows.
pub(crate) async fn get_backfill_route(
	State(services): State<crate::State>,
	ref body: Ruma<get_backfill::v1::Request>,
) -> Result<get_backfill::v1::Response> {
	AccessCheck {
		services: &services,
		origin: body.origin(),
		room_id: &body.room_id,
		event_id: None,
	}
	.check()
	.await?;

	let limit = usize_from_ruma_bounded(body.limit, LIMIT_DEFAULT, LIMIT_MAX);

	// The walk never reaches a requested outlier, so its predecessors join the
	// queue and the timeline events before them place the walk's start.
	let outliers: Vec<PduEvent> = body
		.v
		.iter()
		.stream()
		.wide_filter_map(|event_id| requested_outlier(&services, &body.room_id, event_id))
		.collect()
		.await;

	let walked = body
		.v
		.iter()
		.stream()
		.broad_filter_map(|event_id| {
			services
				.timeline
				.get_pdu_count(event_id)
				.map(Result::ok)
		});

	let preceding = outliers
		.iter()
		.stream()
		.broad_filter_map(|outlier| preceding_count(&services, &body.room_id, limit, outlier));

	let from = walked
		.chain(preceding)
		.ready_fold(PduCount::min(), max)
		.await;

	let room_version = services
		.state
		.get_room_version(&body.room_id)
		.await
		.ok();

	let walk = pin!(
		services
			.timeline
			.pdus_rev(None, &body.room_id, Some(from.saturating_add(1)))
			.map_ok(at!(1))
	);

	let outlier_prevs = outliers
		.iter()
		.flat_map(|outlier| &outlier.prev_events);

	let pending: Pending = body
		.v
		.iter()
		.chain(outlier_prevs)
		.cloned()
		.collect();

	let events =
		unfold((walk, pending, Found::new()), async |(mut walk, mut pending, mut found)| {
			let event = next_event(&services, body, &mut walk, &mut pending, &mut found).await?;

			Some((event, (walk, pending, found)))
		});

	Ok(get_backfill::v1::Response {
		origin_server_ts: MilliSecondsSinceUnixEpoch::now(),

		origin: services.globals.server_name().to_owned(),

		pdus: events
			.ready_try_filter_map(Ok)
			.take(limit)
			.and_then(|pdu| {
				services
					.state_accessor
					.erased_for_server(body.origin(), pdu)
					.map(Ok)
			})
			.and_then(|pdu| {
				services
					.federation
					.format_pdu_into(pdu, room_version.as_ref())
					.map(Ok)
			})
			.try_collect()
			.boxed() // size firewall
			.await?,
	})
}

async fn requested_outlier(
	services: &Services,
	room_id: &RoomId,
	event_id: &EventId,
) -> Option<PduEvent> {
	services
		.timeline
		.get_outlier_pdu(event_id)
		.map(|outlier| {
			outlier
				.ok()
				.filter(|outlier| outlier.room_id == room_id)
		})
		.await
}

/// Returns the count of the latest timeline event preceding an outlier.
///
/// The predecessors are followed through any chain of soft-failed events, for
/// at most `limit` of them, as many as the response can hold. An outlier with
/// no timeline event before it contributes nothing.
async fn preceding_count(
	services: &Services,
	room_id: &RoomId,
	limit: usize,
	outlier: &PduEvent,
) -> Option<PduCount> {
	let prevs = outlier.prev_events.clone();
	let pending: Pending = prevs.iter().cloned().collect();

	let chain = unfold((pending, Found::new()), async |(mut pending, mut found)| {
		let event = next_soft_failed(services, room_id, &mut pending, &mut found).await?;

		pending.extend(event.prev_events.iter().cloned());

		Some((event.prev_events, (pending, found)))
	});

	once(prevs)
		.stream()
		.chain(chain.take(limit))
		.flat_map(|prevs| prevs.into_iter().stream())
		.filter_map(async |prev| services.timeline.get_pdu_count(&prev).await.ok())
		.map(Some)
		.ready_fold(None, max)
		.await
}

/// Takes the next candidate, a found soft-failed event before the walk's next,
/// and yields its JSON when it is served.
///
/// A candidate the origin cannot see yields `None`. One served or named in `v`
/// queues its predecessors, and those that are soft-failed outliers of the room
/// are the next candidates; the request and the events served are the only
/// references that may expose a soft-failed event.
async fn next_event<Walk>(
	services: &Services,
	body: &Ruma<get_backfill::v1::Request>,
	walk: &mut Walk,
	pending: &mut Pending,
	found: &mut Found,
) -> Option<Candidate>
where
	Walk: Stream<Item = Result<PduEvent>> + Unpin,
{
	let event = match next_soft_failed(services, &body.room_id, pending, found).await {
		| Some(event) => event,
		| None => match walk.next().await? {
			| Ok(event) => event,
			| Err(error) => return Some(Err(error)),
		},
	};

	let served = served_json(services, body.origin(), &event).await;

	if served.is_some() || body.v.contains(&event.event_id) {
		pending.extend(event.prev_events.iter().cloned());
	}

	Some(Ok(served))
}

async fn next_soft_failed(
	services: &Services,
	room_id: &RoomId,
	pending: &mut Pending,
	found: &mut Found,
) -> Option<PduEvent> {
	while let Some(event_id) = pending.pop_front() {
		if found.contains(&event_id) {
			continue;
		}

		if let Ok(event) = services
			.pdu_metadata
			.get_soft_failed_outlier(room_id, &event_id)
			.await
		{
			found.insert(event_id);

			return Some(event);
		}
	}

	None
}

#[tracing::instrument(level = "trace", skip_all)]
async fn served_json(
	services: &Services,
	origin: &ServerName,
	event: &PduEvent,
) -> Option<CanonicalJsonObject> {
	services
		.state_accessor
		.server_can_see_event(origin, &event.room_id, &event.event_id)
		.await
		.then_async(|| services.timeline.get_pdu_json(&event.event_id))
		.await
		.and_then(Result::ok)
}
