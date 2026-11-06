use axum::extract::State;
use ruma::api::client::account::request_3pid_management_token_via_email::{self, v3::Response};
use tuwunel_core::{Err, Result};
use tuwunel_service::threepid::canonicalize_email;

use super::email_token::send_email_token;
use crate::{ClientIp, Ruma, utils::check_email_domain};

/// # `POST /_matrix/client/v3/account/3pid/email/requestToken`
///
/// Request a validation token to add an email address to the account.
pub(crate) async fn request_3pid_management_token_via_email_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<request_3pid_management_token_via_email::v3::Request>,
) -> Result<Response> {
	if !services.sendmail.is_enabled() {
		return Err!(Request(ThreepidDenied("Email verification is not configured")));
	}

	let email_canon = canonicalize_email(&body.email)?;

	check_email_domain(&services, &email_canon)?;

	if services
		.threepid
		.address_in_use(&email_canon)
		.await
	{
		return Err!(Request(ThreepidInUse("That email address is already in use")));
	}

	let sid = send_email_token(
		&services,
		client,
		body.client_secret.as_str(),
		&email_canon,
		body.send_attempt.into(),
	)
	.await?;

	Ok(Response::new(sid))
}
