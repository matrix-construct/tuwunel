use axum::extract::State;
use ruma::api::client::session::refresh_token::v3::{Request, Response};
use tuwunel_core::{Err, Result, debug_info, err};
use tuwunel_service::users::device::generate_refresh_token;

use crate::{ClientIp, Ruma};

/// # `POST /_matrix/client/v3/refresh`
///
/// Refresh an access token.
///
/// <https://spec.matrix.org/v1.15/client-server-api/#post_matrixclientv3refresh>
#[tracing::instrument(skip_all, fields(%client), name = "refresh_token")]
pub(crate) async fn refresh_token_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<Request>,
) -> Result<Response> {
	let refresh_token_claim = body.body.refresh_token;

	if !refresh_token_claim.starts_with("refresh_") {
		return Err!(Request(Forbidden("Refresh token is malformed.")));
	}

	let (user_id, device_id, ..) = services
		.users
		.find_from_token(&refresh_token_claim)
		.await
		.map_err(|e| err!(Request(Forbidden("Refresh token is unrecognized: {e}"))))?;

	// New tokens
	let refresh_token = Some(generate_refresh_token());
	let (access_token, expires_in_ms) = services.users.generate_access_token(true);

	services
		.users
		.set_access_token(
			&user_id,
			&device_id,
			&access_token,
			expires_in_ms,
			refresh_token.as_deref(),
		)
		.await?;

	debug_info!(?user_id, ?device_id, ?expires_in_ms, "refreshed their access_token",);

	Ok(Response {
		access_token,
		refresh_token,
		expires_in_ms,
	})
}
