use axum::extract::State;
use futures::StreamExt;
use ruma::OwnedUserId;
use synapse_admin_api::rooms::room_members::v1::{Request, Response};
use tuwunel_core::{Err, Result};

use super::usize_to_uint;
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/rooms/{room_id}/members`
///
/// Lists the joined members of a room, local and remote. No pagination.
pub(crate) async fn admin_room_members_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	if !services.metadata.exists(&body.room_id).await {
		return Err!(Request(NotFound("Room not found")));
	}

	let members: Vec<OwnedUserId> = services
		.state_cache
		.room_members(&body.room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let total = usize_to_uint(members.len());

	Ok(Response { members, total })
}
