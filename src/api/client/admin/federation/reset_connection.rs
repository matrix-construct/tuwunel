use axum::extract::State;
use synapse_admin_api::federation::reset_connection::v1::{Request, Response};
use tuwunel_core::{Err, Result};

use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/federation/destinations/{destination}/reset_connection`
pub(crate) async fn admin_reset_connection_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let destination = &body.destination;

	if services.globals.server_is_ours(destination) {
		return Err!(Request(NotFound("Unknown destination")));
	}

	if services
		.sending
		.notify_peer_alive(destination)
		.await
	{
		return Ok(Response {});
	}

	let known = services
		.state_cache
		.server_shares_room(destination)
		.await;

	if known {
		return Err!(Request(Unknown(
			"The retry timing does not need to be reset for this destination."
		)));
	}

	Err!(Request(NotFound("Unknown destination")))
}
