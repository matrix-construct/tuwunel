use axum::extract::State;
use synapse_admin_api::room_membership::join_room::v1::{Request, Response};
use tuwunel_core::{Err, Result};
use tuwunel_service::membership::Join;

use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/join/{room_id_or_alias}`
///
/// Joins a local user to a room on an admin's behalf, resolving an alias and
/// using the supplied servers as remote-join candidates.
pub(crate) async fn admin_join_room_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let user_id = &body.user_id;

	if !services.globals.user_is_local(user_id) {
		return Err!(Request(InvalidParam("User must be local to this server")));
	}

	if !services.users.exists(user_id).await {
		return Err!(Request(NotFound("Unknown user")));
	}

	let (room_id, servers) = services
		.alias
		.maybe_resolve_with_servers(&body.room_id_or_alias, Some(body.server_name.as_slice()))
		.await?;

	services
		.membership
		.join(Join {
			sender_user: user_id,
			room_id: &room_id,
			orig_room_id: Some(&body.room_id_or_alias),
			reason: None,
			servers: &servers,
			is_appservice: false,
			extra_content: None,
		})
		.await?;

	Ok(Response { room_id })
}
