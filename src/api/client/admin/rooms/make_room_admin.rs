use axum::extract::State;
use synapse_admin_api::rooms::make_room_admin::v1::{Request, Response};
use tuwunel_core::Result;

use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/rooms/{room_id_or_alias}/make_room_admin`
///
/// Grants room admin powers to a user by impersonating the highest-powered
/// local member. The target defaults to the requesting admin.
pub(crate) async fn admin_make_room_admin_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let sender_user = body.sender_user();

	require_admin(&services, sender_user).await?;

	let (room_id, _servers) = services
		.alias
		.maybe_resolve_with_servers(&body.room_id_or_alias, None)
		.await?;

	let target = body.user_id.as_deref().unwrap_or(sender_user);

	services
		.admin
		.make_room_admin(&room_id, target)
		.await?;

	Ok(Response {})
}
