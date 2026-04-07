use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, err, implement, utils};
use tuwunel_database::{Cbor, Deserialized};

#[derive(Debug, Deserialize)]
pub struct DcrRequest {
	pub redirect_uris: Vec<String>,
	pub client_name: Option<String>,
	pub client_uri: Option<String>,
	pub logo_uri: Option<String>,
	#[serde(default)]
	pub contacts: Vec<String>,
	pub token_endpoint_auth_method: Option<String>,
	pub grant_types: Option<Vec<String>>,
	pub response_types: Option<Vec<String>>,
	pub application_type: Option<String>,
	pub policy_uri: Option<String>,
	pub tos_uri: Option<String>,
	pub software_id: Option<String>,
	pub software_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClientRegistration {
	pub client_id: String,
	pub redirect_uris: Vec<String>,
	pub client_name: Option<String>,
	pub client_uri: Option<String>,
	pub logo_uri: Option<String>,
	pub contacts: Vec<String>,
	pub token_endpoint_auth_method: String,
	pub grant_types: Vec<String>,
	pub response_types: Vec<String>,
	pub application_type: Option<String>,
	pub policy_uri: Option<String>,
	pub tos_uri: Option<String>,
	pub software_id: Option<String>,
	pub software_version: Option<String>,
	pub registered_at: u64,
}

const CLIENT_ID_LENGTH: usize = 32;

#[implement(super::Server)]
pub fn register_client(&self, request: DcrRequest) -> Result<ClientRegistration> {
	let client_id = utils::random_string(CLIENT_ID_LENGTH);

	let auth_method = request
		.token_endpoint_auth_method
		.unwrap_or_else(|| "none".to_owned());

	let response_types = request
		.response_types
		.unwrap_or_else(|| vec!["code".to_owned()]);

	let grant_types = request
		.grant_types
		.unwrap_or_else(|| vec!["authorization_code".to_owned(), "refresh_token".to_owned()]);

	let registration = ClientRegistration {
		client_id: client_id.clone(),
		redirect_uris: request.redirect_uris,
		client_name: request.client_name,
		client_uri: request.client_uri,
		logo_uri: request.logo_uri,
		contacts: request.contacts,
		token_endpoint_auth_method: auth_method,
		grant_types,
		response_types,
		application_type: request.application_type,
		policy_uri: request.policy_uri,
		tos_uri: request.tos_uri,
		software_id: request.software_id,
		software_version: request.software_version,
		registered_at: SystemTime::now()
			.duration_since(SystemTime::UNIX_EPOCH)
			.unwrap_or_default()
			.as_secs(),
	};

	self.db
		.oidcclientid_registration
		.raw_put(&*client_id, Cbor(&registration));

	Ok(registration)
}

#[implement(super::Server)]
pub async fn get_client(&self, client_id: &str) -> Result<ClientRegistration> {
	self.db
		.oidcclientid_registration
		.get(client_id)
		.await
		.deserialized::<Cbor<_>>()
		.map(|cbor: Cbor<ClientRegistration>| cbor.0)
		.map_err(|_| err!(Request(NotFound("Unknown client_id"))))
}
