use axum::{Json, extract::State, response::IntoResponse};
use ruma::api::client::discovery::discover_support::{self, Contact};
use serde_json::{Value, json};
use tuwunel_core::{Err, Result};

use crate::Ruma;

/// # `GET /.well-known/matrix/client`
///
/// Returns the .well-known URL if it is configured, otherwise returns 404.
/// Also includes RTC transport configuration for Element Call (MSC4143).
pub(crate) async fn well_known_client(
	State(services): State<crate::State>,
) -> Result<Json<Value>> {
	let client_url = match services.server.config.well_known.client.as_ref() {
		| Some(url) => url.to_string(),
		| None => return Err!(Request(NotFound("Not found."))),
	};

	let mut response = json!({
		"m.homeserver": {
			"base_url": client_url
		}
	});

	// Add RTC transport configuration if available (MSC4143 / Element Call)
	// Element Call has evolved through several versions with different field
	// expectations
	if !services
		.server
		.config
		.well_known
		.rtc_transports
		.is_empty()
	{
		if let Some(obj) = response.as_object_mut() {
			// Element Call expects "org.matrix.msc4143.rtc_foci" (not rtc_foci_preferred)
			// with an array of transport objects
			obj.insert(
				"org.matrix.msc4143.rtc_foci".to_owned(),
				json!(services.server.config.well_known.rtc_transports),
			);

			// Also add the LiveKit URL directly for backward compatibility
			if let Some(first_transport) = services
				.server
				.config
				.well_known
				.rtc_transports
				.first()
			{
				if let Some(livekit_url) = first_transport.get("livekit_service_url") {
					obj.insert(
						"org.matrix.msc4143.livekit_service_url".to_owned(),
						livekit_url.clone(),
					);
				}
			}
		}
	}

	Ok(Json(response))
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
