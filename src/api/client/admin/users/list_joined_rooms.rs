use axum::extract::State;
use futures::StreamExt;
use synapse_admin_api::users::list_joined_rooms::v1 as list_joined_rooms;
use tuwunel_core::{Result, utils::math::ruma_from_usize};

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/users/{user_id}/joined_rooms`
///
/// For remote users this lists only the rooms shared with this server, matching
/// Synapse.
pub(crate) async fn admin_list_joined_rooms_route(
	State(services): State<crate::State>,
	body: Ruma<list_joined_rooms::Request>,
) -> Result<list_joined_rooms::Response> {
	require_admin(&services, body.sender_user()).await?;

	let joined_rooms = services
		.state_cache
		.rooms_joined(&body.user_id)
		.map(ToOwned::to_owned)
		.collect::<Vec<_>>()
		.await;

	let total = ruma_from_usize(joined_rooms.len());

	Ok(list_joined_rooms::Response::new(joined_rooms, total))
}
