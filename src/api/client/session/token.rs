use std::time::Duration;

use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use ruma::{
	OwnedUserId,
	api::client::session::{
		get_login_token,
		login::v3::{Request, Token},
	},
};
use tuwunel_core::{Err, Result, utils::random_string};
use tuwunel_service::Services;

use super::TOKEN_LENGTH;
use crate::{Ruma, router::auth_uiaa};

pub(super) async fn handle_login(
	services: &Services,
	_body: &Ruma<Request>,
	info: &Token,
) -> Result<OwnedUserId> {
	let Token { token } = info;

	if !services.config.login_via_token {
		return Err!(Request(Unknown("Token login is not enabled.")));
	}

	services.users.find_from_login_token(token).await
}

/// # `POST /_matrix/client/v1/login/get_token`
///
/// Allows a logged-in user to get a short-lived token which can be used
/// to log in with the m.login.token flow.
///
/// <https://spec.matrix.org/v1.13/client-server-api/#post_matrixclientv1loginget_token>
#[tracing::instrument(skip_all, fields(%client), name = "login_token")]
pub(crate) async fn login_token_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_login_token::v1::Request>,
) -> Result<get_login_token::v1::Response> {
	if !services.config.login_via_existing_session || !services.config.login_via_token {
		return Err!(Request(Forbidden("Login via an existing session is not enabled")));
	}

	let sender_user = auth_uiaa(&services, &body).await?;
	if !services.users.is_active_local(&sender_user).await {
		return Err!(Request(UserDeactivated("This user has been deactivated.")));
	}

	let login_token = random_string(TOKEN_LENGTH);
	let expires_in = services
		.users
		.create_login_token(&sender_user, &login_token);

	Ok(get_login_token::v1::Response {
		expires_in: Duration::from_millis(expires_in),
		login_token,
	})
}
