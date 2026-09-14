use axum::extract::State;
use ruma::api::{
	client::session::refresh_token::v3::{Request, Response},
	error::{ErrorKind, UnknownTokenErrorData},
};
use tuwunel_core::{
	Err, Error, Result, debug_info,
	utils::{BoolExt, future::OptionFutureExt, time::timepoint_has_passed},
};
use tuwunel_service::users::device::{RefreshToken, generate_refresh_token};

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

	match services
		.users
		.classify_refresh_token(&refresh_token_claim)
		.await
	{
		| RefreshToken::Current { user_id, device_id, expires_at } => {
			if expires_at.is_some_and(timepoint_has_passed) {
				let hard = services.server.config.refresh_token_hard_logout;
				hard.then_async(|| services.users.remove_device(&user_id, &device_id))
					.unwrap_or_else_async(async || {
						services
							.users
							.remove_refresh_token(&user_id, &device_id)
							.await
							.ok();
					})
					.await;

				return Err(Error::BadRequest(
					ErrorKind::UnknownToken(UnknownTokenErrorData { soft_logout: !hard }),
					"Refresh token has expired.",
				));
			}

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
		},

		| RefreshToken::Replayed { user_id, device_id, current, grace } if grace => {
			// Benign double-submit: re-issue an access token for the unchanged
			// refresh token rather than rotating it.
			let (access_token, expires_in_ms) = services.users.generate_access_token(true);

			services
				.users
				.set_access_token(&user_id, &device_id, &access_token, expires_in_ms, None)
				.await?;

			Ok(Response {
				access_token,
				refresh_token: Some(current),
				expires_in_ms,
			})
		},

		| RefreshToken::Replayed { user_id, device_id, .. } => {
			let revoke = services.server.config.refresh_token_reuse_revoke;
			debug_info!(?user_id, ?device_id, revoke, "refresh token reused after rotation");

			if revoke {
				services
					.users
					.remove_device(&user_id, &device_id)
					.await;
			}

			Err(Error::BadRequest(
				ErrorKind::UnknownToken(UnknownTokenErrorData { soft_logout: !revoke }),
				"Refresh token has already been used.",
			))
		},

		| RefreshToken::Unknown => Err!(Request(Forbidden("Refresh token is unrecognized."))),
	}
}
