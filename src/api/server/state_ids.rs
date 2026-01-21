use std::{borrow::Borrow, iter::once};

use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::try_join};
use ruma::{OwnedEventId, api::federation::event::get_room_state_ids};
use tuwunel_core::{Result, at, err};

use super::AccessCheck;
use crate::Ruma;

/// # `GET /_matrix/federation/v1/state_ids/{roomId}`
///
/// Retrieves a snapshot of a room's state at a given event, in the form of
/// event IDs.
pub(crate) async fn get_room_state_ids_route(
	State(services): State<crate::State>,
	body: Ruma<get_room_state_ids::v1::Request>,
) -> Result<get_room_state_ids::v1::Response> {
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
		.map_err(|_| err!(Request(NotFound("Pdu state not found."))));

	let room_version = services.state.get_room_version(&body.room_id);

	let (shortstatehash, room_version) = try_join(shortstatehash, room_version).await?;

	let auth_chain_ids = services
		.auth_chain
		.event_ids_iter(&body.room_id, &room_version, once(body.event_id.borrow()))
		.try_collect();

	let pdu_ids = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.map(at!(1))
		.collect::<Vec<OwnedEventId>>()
		.map(Ok);

	let (auth_chain_ids, pdu_ids) = try_join(auth_chain_ids, pdu_ids).await?;

	Ok(get_room_state_ids::v1::Response { auth_chain_ids, pdu_ids })
}
