use axum::extract::State;
use futures::StreamExt;
use synapse_admin_api::users::memberships::v1 as memberships;
use tuwunel_core::Result;

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/users/{user_id}/memberships`
///
/// For remote users this is limited to rooms the server participates in,
/// matching Synapse.
pub(crate) async fn admin_memberships_route(
	State(services): State<crate::State>,
	body: Ruma<memberships::Request>,
) -> Result<memberships::Response> {
	require_admin(&services, body.sender_user()).await?;

	let map = services
		.state_cache
		.user_memberships(&body.user_id, None)
		.map(|(membership, room_id)| (room_id.to_owned(), membership))
		.collect()
		.await;

	Ok(memberships::Response::new(map))
}
