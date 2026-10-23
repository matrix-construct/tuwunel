use std::borrow::Borrow;

use axum::extract::State;
use futures::{StreamExt, TryFutureExt, future::try_join};
use ruma::{OwnedEventId, api::federation::event::get_room_state_ids};
use tuwunel_core::{Result, at, err};

use super::{
	AccessCheck,
	utils::{require_event_in_room, walk_auth_chain},
};
use crate::Ruma;

/// # `GET /_matrix/federation/v1/state_ids/{roomId}`
///
/// Retrieves a snapshot of a room's state at a given event, in the form of
/// event IDs.
pub(crate) async fn get_room_state_ids_route(
	State(services): State<crate::State>,
	body: Ruma<get_room_state_ids::v1::Request>,
) -> Result<get_room_state_ids::v1::Response> {
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
		.map_err(|_| err!(Request(NotFound("Pdu state not found."))));

	let room_version = services.state.get_room_version(&body.room_id);

	let (shortstatehash, room_version) = try_join(shortstatehash, room_version).await?;

	let pdu_ids: Vec<OwnedEventId> = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.map(at!(1))
		.collect()
		.await;

	let auth_heads = pdu_ids.iter().map(Borrow::borrow);

	let auth_chain_ids = walk_auth_chain(&services, &body.room_id, &room_version, auth_heads)
		.collect()
		.await;

	Ok(get_room_state_ids::v1::Response { auth_chain_ids, pdu_ids })
}
