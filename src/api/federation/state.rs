use std::borrow::Borrow;

use axum::extract::State;
use futures::{
	FutureExt, StreamExt,
	future::{join, try_join},
};
use ruma::{OwnedEventId, api::federation::event::get_room_state};
use tuwunel_core::{
	Result, at, err,
	utils::stream::{BroadbandExt, IterStream},
};

use super::{
	AccessCheck,
	utils::{auth_chain_pdu, require_event_in_room, state_pdu, walk_auth_chain},
};
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

	let auth_chain = walk_auth_chain(&services, &body.room_id, &room_version, auth_heads)
		.broad_filter_map(async |id| auth_chain_pdu(&services, &id, &room_version).await)
		.collect();

	let pdus = state_ids
		.iter()
		.stream()
		.broad_filter_map(|id| state_pdu(&services, id, &room_version))
		.collect();

	let (auth_chain, pdus) = join(auth_chain, pdus).await;

	Ok(get_room_state::v1::Response { auth_chain, pdus })
}
