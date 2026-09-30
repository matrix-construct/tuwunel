//! Appservice registration shared by the server-booting tests that opt in.
//!
//! A test registers its appservice straight through the appservice service, so
//! the token it names authenticates from the next request without a config
//! reload.

use tuwunel_core::{
	Result,
	ruma::api::appservice::{Namespace, Namespaces, Registration, RegistrationInit},
};
use tuwunel_service::Services;

/// The identity and namespaces of one test appservice.
pub(crate) struct Bridge<'a> {
	/// Registration id; the homeserver token is derived from it.
	pub(crate) id: &'a str,

	/// Token the appservice authenticates its requests with.
	pub(crate) token: &'a str,

	/// Localpart of the appservice's own user.
	pub(crate) sender_localpart: &'a str,

	/// Pattern of the user ids it claims.
	pub(crate) users: &'a str,

	/// Pattern of the room aliases it claims, if any.
	pub(crate) aliases: Option<&'a str>,
}

/// Register an appservice directly with the running server.
///
/// Every namespace it claims is exclusive. It claims no rooms and has no URL,
/// since no test here sends it a transaction.
pub(crate) async fn register_appservice(services: &Services, bridge: &Bridge<'_>) -> Result {
	let namespaces = Namespaces {
		users: vec![Namespace::new(true, bridge.users.to_owned())],
		aliases: bridge
			.aliases
			.map(|regex| Namespace::new(true, regex.to_owned()))
			.into_iter()
			.collect(),
		..Default::default()
	};

	let registration: Registration = RegistrationInit {
		id: bridge.id.to_owned(),
		url: None,
		as_token: bridge.token.to_owned(),
		hs_token: format!("{}-hs-token", bridge.id),
		sender_localpart: bridge.sender_localpart.to_owned(),
		namespaces,
		rate_limited: None,
		protocols: None,
	}
	.into();

	services
		.appservice
		.register_appservice(registration)
		.await
}
