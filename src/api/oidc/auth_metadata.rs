use axum::{Json, extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};
use tuwunel_core::Result;

const ACCOUNT_MANAGEMENT_ACTIONS_SUPPORTED: &[&str] = &[
	"org.matrix.profile",
	"org.matrix.sessions_list",
	"org.matrix.session_view",
	"org.matrix.session_end",
];

#[derive(Debug, Serialize, Deserialize)]
struct ProviderMetadata {
	issuer: String,
	authorization_endpoint: String,
	token_endpoint: String,
	registration_endpoint: Option<String>,
	revocation_endpoint: Option<String>,
	jwks_uri: String,
	userinfo_endpoint: Option<String>,
	account_management_uri: Option<String>,
	account_management_actions_supported: Option<Vec<String>>,
	response_types_supported: Vec<String>,
	response_modes_supported: Option<Vec<String>>,
	grant_types_supported: Option<Vec<String>>,
	code_challenge_methods_supported: Option<Vec<String>>,
	token_endpoint_auth_methods_supported: Option<Vec<String>>,
	scopes_supported: Option<Vec<String>>,
	subject_types_supported: Option<Vec<String>>,
	id_token_signing_alg_values_supported: Option<Vec<String>>,
	prompt_values_supported: Option<Vec<String>>,
	claim_types_supported: Option<Vec<String>>,
	claims_supported: Option<Vec<String>>,
}

pub(crate) async fn openid_configuration_route(
	State(services): State<crate::State>,
) -> Result<impl IntoResponse> {
	let issuer = services.oauth.get_server()?.issuer_url()?;
	let base = issuer.trim_end_matches('/').to_owned();
	let account_management_available = services.config.identity_provider.len() == 1;

	Ok(Json(ProviderMetadata {
		issuer,

		authorization_endpoint: format!("{base}/_tuwunel/oidc/authorize"),

		registration_endpoint: Some(format!("{base}/_tuwunel/oidc/registration")),

		userinfo_endpoint: Some(format!("{base}/_tuwunel/oidc/userinfo")),

		token_endpoint: format!("{base}/_tuwunel/oidc/token"),

		jwks_uri: format!("{base}/_tuwunel/oidc/jwks"),

		account_management_uri: account_management_available
			.then_some(format!("{base}/_tuwunel/oidc/account")),

		revocation_endpoint: Some(format!("{base}/_tuwunel/oidc/revoke")),

		response_modes_supported: Some(vec!["query".to_owned()]),

		response_types_supported: vec!["code".to_owned()],

		code_challenge_methods_supported: Some(vec!["S256".to_owned()]),

		id_token_signing_alg_values_supported: Some(vec!["ES256".to_owned()]),

		// "create" prompt is not implemented; omit to avoid misleading clients.
		prompt_values_supported: None,

		subject_types_supported: Some(vec!["public".to_owned()]),

		claim_types_supported: Some(vec!["normal".to_owned()]),

		grant_types_supported: Some(vec![
			"authorization_code".to_owned(),
			"refresh_token".to_owned(),
		]),

		token_endpoint_auth_methods_supported: Some(vec![
			"none".to_owned(),
			"client_secret_basic".to_owned(),
			"client_secret_post".to_owned(),
		]),

		scopes_supported: Some(vec![
			"openid".to_owned(),
			"urn:matrix:org.matrix.msc2967.client:api:*".to_owned(),
			"urn:matrix:org.matrix.msc2967.client:device:*".to_owned(),
		]),

		account_management_actions_supported: account_management_available.then(|| {
			ACCOUNT_MANAGEMENT_ACTIONS_SUPPORTED
				.iter()
				.map(ToString::to_string)
				.collect()
		}),

		claims_supported: Some(vec![
			"iss".to_owned(),
			"sub".to_owned(),
			"aud".to_owned(),
			"exp".to_owned(),
			"iat".to_owned(),
			"nonce".to_owned(),
		]),
	}))
}
