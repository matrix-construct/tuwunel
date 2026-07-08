use axum::extract::State;
use synapse_admin_api::rooms::block::{
	get::{Request as GetRequest, Response as GetResponse},
	set::{Request as SetRequest, Response as SetResponse},
};
use tuwunel_core::{Result, utils::BoolExt};

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/rooms/{room_id}/block`
///
/// Reports whether a room is blocked and, when it is, the admin that blocked
/// it. The blocker is omitted for rooms blocked before the mxid was recorded.
pub(crate) async fn admin_get_room_block_route(
	State(services): State<crate::State>,
	body: Ruma<GetRequest>,
) -> Result<GetResponse> {
	require_admin(&services, body.sender_user()).await?;

	let block = services.metadata.is_banned(&body.room_id).await;

	let user_id = block
		.then_async(|| {
			services
				.metadata
				.banned_room_blocker(&body.room_id)
		})
		.await
		.flatten();

	Ok(GetResponse { block, user_id })
}

/// # `PUT /_synapse/admin/v1/rooms/{room_id}/block`
///
/// Blocks or unblocks a room, recording the requesting admin as the blocker.
/// Pre-emptively blocking a room the server does not know is allowed.
pub(crate) async fn admin_set_room_block_route(
	State(services): State<crate::State>,
	body: Ruma<SetRequest>,
) -> Result<SetResponse> {
	let sender_user = body.sender_user();

	require_admin(&services, sender_user).await?;

	match body.block {
		| true => services
			.metadata
			.block_room(&body.room_id, sender_user),
		| false => services.metadata.unban_room(&body.room_id),
	}

	Ok(SetResponse { block: body.block })
}
