use axum::extract::State;
use ruma::{
	MilliSecondsSinceUnixEpoch,
	api::client::account::add_3pid::{self, v3::Response},
};
use tuwunel_core::{Err, Result, err, utils::BoolExt};

use crate::{ClientIp, Ruma, router::auth_uiaa};

/// # `POST /_matrix/client/v3/account/3pid/add`
///
/// Bind a verified email address to this account.
///
/// - Requires UIAA to confirm account ownership
/// - Consumes a previously validated email verification session
#[tracing::instrument(skip_all, fields(%client), name = "add_3pid")]
pub(crate) async fn add_3pid_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<add_3pid::v3::Request>,
) -> Result<Response> {
	if !services.sendmail.is_enabled() {
		return Err!(Request(ThreepidDenied("Email verification is not configured")));
	}

	let ref sender_user = auth_uiaa(&services, &body).await?;

	let association = services
		.threepid
		.redeem_validated(body.sid.as_str(), body.client_secret.as_str())
		.await?;

	let now = MilliSecondsSinceUnixEpoch::now();

	services
		.threepid
		.bind_unclaimed(sender_user, &association.address, association.medium, now)
		.await?
		.into_option()
		.ok_or_else(|| err!(Request(ThreepidInUse("That email address is already in use"))))?;

	Ok(Response::new())
}
