use std::{borrow::Borrow, iter::once};

use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::try_join};
use ruma::{OwnedEventId, api::federation::event::get_room_state};
use tuwunel_core::{
	Result, at, err,
	utils::stream::{IterStream, TryBroadbandExt},
};

use super::AccessCheck;
use crate::Ruma;

/// # `GET /_matrix/federation/v1/state/{roomId}`
///
/// Retrieves a snapshot of a room's state at a given event.
pub(crate) async fn get_room_state_route(
	State(services): State<crate::State>,
	body: Ruma<get_room_state::v1::Request>,
) -> Result<get_room_state::v1::Response> {
	AccessCheck {
		services: &services,
		origin: body.origin(),
		room_id: &body.room_id,
		event_id: None,
	}
	.check()
	.await?;

	let shortstatehash = services
		.state
		.pdu_shortstatehash(&body.event_id)
		.await
		.map_err(|_| err!(Request(NotFound("PDU state not found."))))?;

	let state_ids = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.map(at!(1))
		.collect::<Vec<OwnedEventId>>()
		.map(Ok);

	let room_version = services.state.get_room_version(&body.room_id);

	let (room_version, state_ids) = try_join(room_version, state_ids).await?;

	let into_federation_format = |pdu| {
		services
			.federation
			.format_pdu_into(pdu, Some(&room_version))
			.map(Ok)
	};

	let auth_chain = services
		.auth_chain
		.event_ids_iter(&body.room_id, &room_version, once(body.event_id.borrow()))
		.broad_and_then(async |id| {
			services
				.timeline
				.get_pdu_json(&id)
				.and_then(into_federation_format)
				.await
		})
		.try_collect();

	let pdus = state_ids
		.iter()
		.try_stream()
		.broad_and_then(|id| {
			services
				.timeline
				.get_pdu_json(id)
				.and_then(into_federation_format)
		})
		.try_collect();

	let (auth_chain, pdus) = try_join(auth_chain, pdus).await?;

	Ok(get_room_state::v1::Response { auth_chain, pdus })
}
