use axum::extract::State;
use futures::StreamExt;
use ruma::{
	OwnedUserId,
	api::client::{
		account::change_password,
		uiaa::{AuthData, EmailIdentity, ThirdpartyIdCredentials},
	},
	thirdparty::Medium,
};
use tuwunel_core::{Err, Error, Result, info, utils::ReadyExt};

use crate::{ClientIp, Ruma, router::auth_uiaa};

/// # `POST /_matrix/client/r0/account/password`
///
/// Changes the password of this account.
///
/// - Authenticated changes require UIAA to verify the current user
/// - Logged-out resets consume a validated email proof and derive the target
///   from its reverse binding
/// - Changes the password of the authenticated or proof-bound user
/// - The password hash is calculated using argon2 with 32 character salt, the
///   plain password is
/// not saved
///
/// If `logout_devices` is true, authenticated changes apply the following
/// actions to each device except the sender device. Logged-out resets apply
/// them to every device:
/// - Invalidates access token
/// - Deletes device metadata (device id, device display name, last seen ip,
///   last seen ts)
/// - Forgets to-device events
/// - Triggers device list updates
#[tracing::instrument(skip_all, fields(%client), name = "change_password")]
pub(crate) async fn change_password_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<change_password::v3::Request>,
) -> Result<change_password::v3::Response> {
	let sender_user = match (body.sender_user.as_ref(), body.auth.as_ref()) {
		| (None, Some(AuthData::EmailIdentity(EmailIdentity { thirdparty_id_creds, .. }))) =>
			redeem_password_reset(services, thirdparty_id_creds).await?,
		| (None, _) => return Err!(Request(MissingToken("Missing access token."))),
		| (Some(_), _) => auth_uiaa(&services, &body).await?,
	};

	services
		.users
		.set_password(&sender_user, Some(&body.new_password))
		.await?;

	if body.logout_devices {
		// A logged-out reset has no current device to preserve.
		services
			.users
			.all_device_ids(&sender_user)
			.ready_filter(|&id| Some(id) != body.sender_device.as_deref())
			.for_each(|id| services.users.remove_device(&sender_user, id))
			.await;
	}

	info!("User {sender_user} changed their password.");

	services
		.admin
		.notify(&format!("User {sender_user} changed their password."))
		.await;

	Ok(change_password::v3::Response {})
}

#[tracing::instrument(level = "debug", skip_all)]
async fn redeem_password_reset(
	services: crate::State,
	creds: &ThirdpartyIdCredentials,
) -> Result<OwnedUserId> {
	let association = match services
		.threepid
		.redeem_validated(creds.sid.as_str(), creds.client_secret.as_str())
		.await
	{
		| Ok(association) => association,
		| Err(error) if error.is_not_found() || matches!(&error, Error::Request(..)) => {
			return Err!(Request(Forbidden("Invalid email identity proof.")));
		},
		| Err(error) => return Err(error),
	};

	if association.medium != Medium::Email {
		return Err!(Request(Forbidden("Invalid email identity proof.")));
	}

	let Some(user_id) = services
		.threepid
		.user_id_for_email(&association.address)
		.await?
	else {
		return Err!(Request(Forbidden("Invalid email identity proof.")));
	};

	Ok(user_id)
}
