mod available;
mod register;
mod token_validity;

use tuwunel_core::{Error, err};
use tuwunel_service::appservice::RegistrationInfo;

pub(crate) use self::{
	available::get_register_available_route, register::register_route,
	token_validity::check_registration_token_validity,
};
use super::SESSION_ID_LENGTH;
use crate::State;

/// Why a caller cannot register an account by name.
#[derive(Clone, Copy, Debug)]
pub(super) enum Closed {
	/// Registration is disabled.
	Registration,

	/// Registration is enabled, but not with a password.
	Password,
}

impl From<Closed> for Error {
	fn from(closed: Closed) -> Self {
		match closed {
			| Closed::Registration => err!(Request(Forbidden("Registration has been disabled."))),
			| Closed::Password =>
				err!(Request(Forbidden("Password registration has been disabled."))),
		}
	}
}

/// Refuses a caller who cannot register an account by name.
///
/// `POST /register` refuses a named account on it, and the availability check
/// refuses on it before reading the name, so the two answer such a caller alike
/// for every name. A caller holding an appservice token is never refused.
pub(super) fn check_registration_open(
	services: State,
	appservice_info: Option<&RegistrationInfo>,
) -> Result<(), Closed> {
	match appservice_info {
		| None if !services.config.registration.enable => Err(Closed::Registration),
		| None if !services.config.allows_password_registration() => Err(Closed::Password),
		| _ => Ok(()),
	}
}

// workaround for https://github.com/matrix-org/matrix-appservice-irc/issues/1780
pub(super) fn is_matrix_appservice_irc(appservice_info: Option<&RegistrationInfo>) -> bool {
	appservice_info.is_some_and(|appservice| {
		let id = &appservice.registration.id;
		id == "irc"
			|| id.contains("matrix-appservice-irc")
			|| id.contains("matrix_appservice_irc")
	})
}
