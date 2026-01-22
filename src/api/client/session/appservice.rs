use ruma::{
	OwnedUserId, UserId,
	api::client::{
		session::login::v3::{ApplicationService, Request},
		uiaa,
	},
};
use tuwunel_core::{Err, Result, err};
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

	if !services.globals.user_is_local(&user_id) {
		return Err!(Request(Unknown("User ID does not belong to this homeserver")));
	}

	let emergency_mode_enabled = services.config.emergency_password.is_some();

	if !info.is_user_match(&user_id) && !emergency_mode_enabled {
		return Err!(Request(Exclusive("Username is not in an appservice namespace.")));
	}

	Ok(user_id)
}
