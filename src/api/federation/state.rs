use axum::extract::State;
use futures::{StreamExt, future::join};
use ruma::api::federation::event::get_room_state;
use tuwunel_core::{
	Result,
	utils::stream::{BroadbandExt, IterStream},
};

use super::utils::{auth_chain_pdu, state_at_event, state_pdu, walk_state_auth_chain};
use crate::Ruma;

/// # `GET /_matrix/federation/v1/state/{roomId}`
///
/// Retrieves a snapshot of a room's state at a given event.
pub(crate) async fn get_room_state_route(
	State(services): State<crate::State>,
	body: Ruma<get_room_state::v1::Request>,
) -> Result<get_room_state::v1::Response> {
	let (room_version, state_ids) =
		state_at_event(&services, body.origin(), &body.room_id, &body.event_id).await?;

	let auth_chain = walk_state_auth_chain(
		&services,
		&body.room_id,
		&room_version,
		&state_ids,
		&body.event_id,
	)
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
