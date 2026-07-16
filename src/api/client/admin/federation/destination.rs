use axum::extract::State;
use synapse_admin_api::federation::destination::v1::{Request, Response};
use tuwunel_core::{Err, Result};

use super::destination_from_backoff;
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/federation/destinations/{destination}`
pub(crate) async fn admin_destination_details_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	if services.globals.server_is_ours(&body.destination) {
		return Err!(Request(NotFound("Unknown destination")));
	}

	let backoff = services
		.federation
		.peer_backoff(&body.destination)
		.await;

	if backoff.is_none()
		&& !services
			.state_cache
			.server_shares_room(&body.destination)
			.await
	{
		return Err!(Request(NotFound("Unknown destination")));
	}

	let destination = destination_from_backoff(body.body.destination, backoff);

	Ok(Response { destination })
}
