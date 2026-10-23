use axum::extract::State;
use futures::StreamExt;
use ruma::api::federation::event::get_room_state_ids;
use tuwunel_core::Result;

use super::utils::{state_at_event, walk_state_auth_chain};
use crate::Ruma;

/// # `GET /_matrix/federation/v1/state_ids/{roomId}`
///
/// Retrieves a snapshot of a room's state at a given event, in the form of
/// event IDs.
pub(crate) async fn get_room_state_ids_route(
	State(services): State<crate::State>,
	body: Ruma<get_room_state_ids::v1::Request>,
) -> Result<get_room_state_ids::v1::Response> {
	let (room_version, pdu_ids) =
		state_at_event(&services, body.origin(), &body.room_id, &body.event_id).await?;

	let auth_chain_ids =
		walk_state_auth_chain(&services, &body.room_id, &room_version, &pdu_ids, &body.event_id)
			.collect()
			.await;

	Ok(get_room_state_ids::v1::Response { auth_chain_ids, pdu_ids })
}
