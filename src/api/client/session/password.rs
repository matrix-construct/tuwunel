use std::net::IpAddr;

use futures::{FutureExt, TryFutureExt};
use ruma::{
	OwnedUserId, UserId,
	api::client::{
		session::login::v3::{Password, Request},
		uiaa,
	},
};
use tuwunel_core::{Err, Result, err};
use tuwunel_service::Services;

use super::ldap_login;
use crate::Ruma;

pub(super) async fn handle_login(
	services: &Services,
	body: &Ruma<Request>,
	info: &Password,
	client: IpAddr,
) -> Result<OwnedUserId> {
	#[expect(deprecated)]
	let Password { identifier, password, user, .. } = info;

	let user_id = if let Some(uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier {
		user: user_id,
		..
	})) = identifier
	{
		UserId::parse_with_server_name(user_id, &services.config.server_name)
	} else if let Some(user) = user {
		UserId::parse_with_server_name(user, &services.config.server_name)
	} else {
		return Err!(Request(Unknown(debug_warn!(
			?body.login_info,
			"Valid identifier or username was not provided (invalid or unsupported login type?)"
		))));
	}
	.map_err(|e| err!(Request(InvalidUsername(warn!("Username is invalid: {e}")))))?;

	let lowercased_user_id = UserId::parse_with_server_name(
		user_id.localpart().to_lowercase(),
		&services.config.server_name,
	)?;

	let user_is_remote = !services.globals.user_is_local(&user_id)
		|| !services
			.globals
			.user_is_local(&lowercased_user_id);

	if user_is_remote {
		return Err!(Request(Unknown("User ID does not belong to this homeserver")));
	}

	if cfg!(feature = "ldap") && services.config.ldap.enable {
		ldap_login(services, &user_id, &lowercased_user_id, password, client)
			.boxed() // cold arm
			.await
	} else {
		password_login(services, &user_id, password, client).await
	}
}

/// Authenticates the given user by its ID and its password.
///
/// Returns the user ID if successful. A wrong password, a name matching no
/// account and an account that cannot sign in by password all get one error.
#[tracing::instrument(skip_all, fields(%user_id), name = "password")]
pub(super) async fn password_login(
	services: &Services,
	user_id: &UserId,
	password: &str,
	client: IpAddr,
) -> Result<OwnedUserId> {
	services
		.users
		.password_login(user_id, password, client)
		.map_ok(|(user_id, _hash)| user_id)
		.await
}
