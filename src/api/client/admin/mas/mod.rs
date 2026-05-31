mod allow_cross_signing_reset;
mod delete_device;
mod delete_user;
mod is_localpart_available;
mod provision_user;
mod query_user;
mod reactivate_user;
mod set_displayname;
mod sync_devices;
mod unset_displayname;
mod update_device_display_name;
mod upsert_device;

use axum::{RequestPartsExt, extract::FromRequestParts};
use axum_extra::{
	TypedHeader,
	headers::{Authorization, authorization::Bearer},
};
use http::request::Parts;
use ruma::{OwnedUserId, UserId};
use tuwunel_core::{Err, Error, Result, err};

pub(crate) use self::{
	allow_cross_signing_reset::allow_cross_signing_reset_route,
	delete_device::delete_device_route, delete_user::delete_user_route,
	is_localpart_available::is_localpart_available_route, provision_user::provision_user_route,
	query_user::query_user_route, reactivate_user::reactivate_user_route,
	set_displayname::set_displayname_route, sync_devices::sync_devices_route,
	unset_displayname::unset_displayname_route,
	update_device_display_name::update_device_display_name_route,
	upsert_device::upsert_device_route,
};

/// Asserts a request originates from MAS by matching its bearer token against
/// the configured shared secret, the equivalent of Synapse's
/// `assert_request_is_from_mas`. Rejects with 403 on mismatch or no secret.
pub(crate) struct Mas;

impl FromRequestParts<crate::State> for Mas {
	type Rejection = Error;

	async fn from_request_parts(
		parts: &mut Parts,
		services: &crate::State,
	) -> Result<Self, Self::Rejection> {
		let secret = services
			.config
			.mas_secret
			.as_deref()
			.filter(|secret| !secret.is_empty());

		let bearer = parts
			.extract::<TypedHeader<Authorization<Bearer>>>()
			.await
			.ok();

		let token = bearer
			.as_ref()
			.map(|TypedHeader(Authorization(bearer))| bearer.token());

		match (secret, token) {
			| (Some(secret), Some(token)) if secret == token => Ok(Self),
			| _ => Err!(Request(Forbidden("This endpoint may only be called by MAS"))),
		}
	}
}

/// Parses a MAS `localpart` into a local user id, rejecting a malformed one
/// with `400`.
pub(super) fn local_user(services: crate::State, localpart: &str) -> Result<OwnedUserId> {
	UserId::parse_with_server_name(localpart, services.globals.server_name())
		.map_err(|_| err!(Request(InvalidParam("Invalid localpart"))))
}

/// Resolves a MAS `localpart` to an existing local user, rejecting an absent
/// one with `404`.
pub(super) async fn existing_user(
	services: crate::State,
	localpart: &str,
) -> Result<OwnedUserId> {
	let user_id = local_user(services, localpart)?;

	services
		.users
		.exists(&user_id)
		.await
		.then_some(user_id)
		.ok_or_else(|| err!(Request(NotFound("User does not exist"))))
}
