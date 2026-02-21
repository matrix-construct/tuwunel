use axum::{Json, extract::State, response::IntoResponse};
use ruma::api::client::discovery::{
	discover_homeserver::{self, HomeserverInfo, RtcFocusInfo},
	discover_support::{self, Contact},
};
use serde_json::Value as JsonValue;
use tuwunel_core::{Err, Result, err, error::inspect_log};

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
		base_url: match services.server.config.well_known.client.as_ref() {
			| Some(url) => url.to_string(),
			| None => return Err!(Request(NotFound("Not found."))),
		},
	};

	// Add RTC transport configuration if available (MSC4143 / Element Call)
	// Element Call has evolved through several versions with different field
	// expectations
	let mut rtc_foci: Vec<_> = services
		.server
		.config
		.well_known
		.rtc_transports
		.iter()
		.map(|transport| {
			let focus_type = transport
				.get("type")
				.and_then(JsonValue::as_str)
				.ok_or_else(|| err!("`type` is not a valid string"))?;

			let transport = transport
				.as_object()
				.cloned()
				.ok_or_else(|| err!("`rtc_transport` is not a valid object"))?;

			RtcFocusInfo::new(focus_type, transport).map_err(Into::into)
		})
		.collect::<Result<_>>()
		.map_err(|e| {
			err!(Config("global.well_known.rtc_transports", "Malformed value(s): {e:?}"))
		})
		.inspect_err(inspect_log)?;

	if let Some(livekit_url) = &services.config.well_known.livekit_url {
		rtc_foci.push(RtcFocusInfo::livekit(livekit_url.clone()));
	}

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
	let support_page = services
		.server
		.config
		.well_known
		.support_page
		.as_ref()
		.map(ToString::to_string);

	let role = services
		.server
		.config
		.well_known
		.support_role
		.clone();

	// support page or role must be either defined for this to be valid
	if support_page.is_none() && role.is_none() {
		return Err!(Request(NotFound("Not found.")));
	}

	let email_address = services
		.server
		.config
		.well_known
		.support_email
		.clone();

	let matrix_id = services
		.server
		.config
		.well_known
		.support_mxid
		.clone();

	// if a role is specified, an email address or matrix id is required
	if role.is_some() && (email_address.is_none() && matrix_id.is_none()) {
		return Err!(Request(NotFound("Not found.")));
	}

	// TODO: support defining multiple contacts in the config
	let mut contacts: Vec<Contact> = vec![];

	if let Some(role) = role {
		let contact = Contact { role, email_address, matrix_id };

		contacts.push(contact);
	}

	// support page or role+contacts must be either defined for this to be valid
	if contacts.is_empty() && support_page.is_none() {
		return Err!(Request(NotFound("Not found.")));
	}

	Ok(discover_support::Response { contacts, support_page })
}

/// # `GET /client/server.json`
///
/// Endpoint provided by sliding sync proxy used by some clients such as Element
/// Web as a non-standard health check.
pub(crate) async fn syncv3_client_server_json(
	State(services): State<crate::State>,
) -> Result<impl IntoResponse> {
	let server_url = match services.server.config.well_known.client.as_ref() {
		| Some(url) => url.to_string(),
		| None => match services.server.config.well_known.server.as_ref() {
			| Some(url) => url.to_string(),
			| None => return Err!(Request(NotFound("Not found."))),
		},
	};

	Ok(Json(serde_json::json!({
		"server": server_url,
		"version": tuwunel_core::version(),
	})))
}
