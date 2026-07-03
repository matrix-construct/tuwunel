use std::cmp;

use axum::extract::State;
use futures::{FutureExt, StreamExt, TryStreamExt};
use ruma::{MilliSecondsSinceUnixEpoch, api::federation::backfill::get_backfill};
use tuwunel_core::{
	PduCount, Result,
	utils::{IterStream, ReadyExt},
};

use super::AccessCheck;
use crate::Ruma;

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

	let limit = body
		.limit
		.try_into()
		.unwrap_or(LIMIT_DEFAULT)
		.min(LIMIT_MAX);

	let from = body
		.v
		.iter()
		.stream()
		.filter_map(|event_id| {
			services
				.timeline
				.get_pdu_count(event_id)
				.map(Result::ok)
		})
		.ready_fold(PduCount::min(), cmp::max)
		.await;

	let room_version = services
		.state
		.get_room_version(&body.room_id)
		.await
		.ok();

	Ok(get_backfill::v1::Response {
		origin_server_ts: MilliSecondsSinceUnixEpoch::now(),

		origin: services.globals.server_name().to_owned(),

		pdus: services
			.timeline
			.pdus_rev(None, &body.room_id, Some(from.saturating_add(1)))
			.try_filter_map(async |(_, pdu)| {
				Ok(services
					.state_accessor
					.server_can_see_event(body.origin(), &pdu.room_id, &pdu.event_id)
					.await
					.then_some(pdu))
			})
			.try_filter_map(async |pdu| {
				Ok(services
					.timeline
					.get_pdu_json(&pdu.event_id)
					.await
					.ok())
			})
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
			.boxed()
			.await?,
	})
}
