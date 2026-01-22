use futures::{FutureExt, TryFutureExt};
use ruma::{
	OwnedUserId, UserId,
	api::client::{
		session::login::v3::{Password, Request},
		uiaa,
	},
};
use tuwunel_core::{Err, Result, debug_error, err, utils::hash};
use tuwunel_service::Services;

use super::ldap_login;
use crate::Ruma;

pub(super) async fn handle_login(
	services: &Services,
	body: &Ruma<Request>,
	info: &Password,
) -> Result<OwnedUserId> {
	#[expect(deprecated)]
	let Password { identifier, password, user, .. } = info;

	let user_id = if let Some(uiaa::UserIdentifier::UserIdOrLocalpart(user_id)) = identifier {
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
		ldap_login(services, &user_id, &lowercased_user_id, password)
			.boxed()
			.await
	} else {
		password_login(services, &user_id, &lowercased_user_id, password).await
	}
}

/// Authenticates the given user by its ID and its password.
///
/// Returns the user ID if successful, and an error otherwise.
#[tracing::instrument(skip_all, fields(%user_id), name = "password")]
pub(super) async fn password_login(
	services: &Services,
	user_id: &UserId,
	lowercased_user_id: &UserId,
	password: &str,
) -> Result<OwnedUserId> {
	// Restrict login to accounts only of type 'password', including untyped
	// legacy accounts which are equivalent to 'password'.
	if services
		.users
		.origin(user_id)
		.await
		.is_ok_and(|origin| origin != "password")
	{
		return Err!(Request(Forbidden("Account does not permit password login.")));
	}

	let (hash, user_id) = services
		.users
		.password_hash(user_id)
		.map_ok(|hash| (hash, user_id))
		.or_else(|_| {
			services
				.users
				.password_hash(lowercased_user_id)
				.map_ok(|hash| (hash, lowercased_user_id))
		})
		.map_err(|_| err!(Request(Forbidden("Wrong username or password."))))
		.await?;

	if hash.is_empty() {
		return Err!(Request(UserDeactivated("The user has been deactivated")));
	}

	hash::verify_password(password, &hash)
		.inspect_err(|e| debug_error!("{e}"))
		.map_err(|_| err!(Request(Forbidden("Wrong username or password."))))?;

	Ok(user_id.to_owned())
}
