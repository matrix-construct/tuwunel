use axum::extract::State;
use ruma::api::client::discovery::{
	discover_homeserver::{self, HomeserverInfo},
	discover_support,
};
use tuwunel_core::{Err, Result};

use crate::Ruma;

/// # `GET /.well-known/matrix/client`
///
/// Returns the .well-known URL if it is configured, otherwise returns 404.
/// Also includes RTC transport configuration for Element Call (MSC4143).
pub(crate) async fn well_known_client(
	State(services): State<crate::State>,
	_body: Ruma<discover_homeserver::Request>,
) -> Result<discover_homeserver::Response> {
	let homeserver = HomeserverInfo {
		base_url: match services.config.well_known.client.as_ref() {
			| Some(url) => url.to_string(),
			| None => return Err!(Request(NotFound("Not found."))),
		},
	};

	let rtc_foci = services.config.rtc_transports()?;

	Ok(discover_homeserver::Response {
		rtc_foci,
		..discover_homeserver::Response::new(homeserver)
	})
}

/// # `GET /.well-known/matrix/support`
///
/// Server support contact and support page of a homeserver's domain.
pub(crate) async fn well_known_support(
	State(services): State<crate::State>,
	_body: Ruma<discover_support::Request>,
) -> Result<discover_support::Response> {
	let config = &services.config.well_known;

	let support_page = config
		.support_page
		.as_ref()
		.map(ToString::to_string);

	let contacts = config.get_contacts();

	let policies = config.get_policies();

	if support_page.is_none() && contacts.is_empty() && policies.is_empty() {
		return Err!(Request(NotFound("Not found.")));
	}

	Ok(discover_support::Response { contacts, support_page, policies })
}
