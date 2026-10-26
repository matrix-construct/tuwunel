use ruma::{
	OwnedUserId, UserId,
	api::client::{
		session::login::v3::{ApplicationService, Request},
		uiaa,
	},
};
use tuwunel_core::{Err, Result, err, extract};
use tuwunel_service::Services;

use crate::Ruma;

pub(super) fn handle_login(
	services: &Services,
	body: &Ruma<Request>,
	info: &ApplicationService,
) -> Result<OwnedUserId> {
	#[expect(deprecated)]
	let ApplicationService { identifier, user } = info;

	let Some(ref info) = body.appservice_info else {
		return Err!(Request(MissingToken("Missing appservice token.")));
	};

	// MSC4190: an appservice managing its own devices does not use appservice
	// login; it creates devices directly via PUT /devices/{deviceId}.
	if info.registration.device_management {
		return Err!(Request(AppserviceLoginUnsupported(
			"Appservice has MSC4190 device management enabled; appservice login is unsupported."
		)));
	}

	let user_id = extract!(
		identifier,
		x in Some(uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier { user: x, .. }))
	)
	.or(user.as_ref())
	.ok_or_else(|| {
		err!(Request(Unknown(debug_warn!(
			?body.login_info,
			"Valid identifier or username was not provided (invalid or unsupported login type?)"
		))))
	})?;

	let user_id = UserId::parse_with_server_name(user_id, &services.config.server_name)
		.map_err(|e| err!(Request(InvalidUsername(warn!("Username is invalid: {e}")))))?;

	if !services.globals.user_is_local(&user_id) {
		return Err!(Request(Unknown("User ID does not belong to this homeserver")));
	}

	let emergency_mode_enabled = services
		.config
		.authentication
		.emergency_password
		.is_some();

	if !info.is_user_match(&user_id) && !emergency_mode_enabled {
		return Err!(Request(Exclusive("Username is not in an appservice namespace.")));
	}

	Ok(user_id)
}
