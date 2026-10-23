use std::borrow::Borrow;

use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::try_join};
use ruma::{OwnedEventId, api::federation::event::get_room_state};
use tuwunel_core::{
	Result, at, err,
	result::NotFound,
	utils::{
		future::TryExtExt,
		stream::{IterStream, TryBroadbandExt, TryReadyExt},
	},
	warn,
};

use super::{AccessCheck, utils::require_event_in_room};
use crate::Ruma;

/// # `GET /_matrix/federation/v1/state/{roomId}`
///
/// Retrieves a snapshot of a room's state at a given event.
pub(crate) async fn get_room_state_route(
	State(services): State<crate::State>,
	body: Ruma<get_room_state::v1::Request>,
) -> Result<get_room_state::v1::Response> {
	let access_check = AccessCheck {
		services: &services,
		origin: body.origin(),
		room_id: &body.room_id,
		event_id: None,
	};

	access_check.check().await?;

	require_event_in_room(&services, &body.event_id, &body.room_id).await?;

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

	let auth_heads = state_ids.iter().map(Borrow::borrow);

	let auth_chain = services
		.auth_chain
		.event_ids_iter(&body.room_id, &room_version, auth_heads)
		.broad_and_then(async |id| {
			services
				.federation
				.get_pdu_into(&id, Some(&room_version))
				.map(NotFound::optional)
				.inspect_err(|error| warn!(?id, %error, "Omitting unreadable auth event."))
				.ok()
				.map(Option::flatten)
				.map(Ok)
				.await
		})
		.ready_try_filter_map(Ok)
		.try_collect();

	let pdus = state_ids
		.iter()
		.try_stream()
		.broad_and_then(|id| {
			services
				.federation
				.get_pdu_into(id, Some(&room_version))
		})
		.try_collect();

	let (auth_chain, pdus) = try_join(auth_chain, pdus).await?;

	Ok(get_room_state::v1::Response { auth_chain, pdus })
}
