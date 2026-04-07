pub(super) mod account;
pub(super) mod auth_issuer;
pub(super) mod auth_metadata;
pub(super) mod authorize;
pub(super) mod complete;
pub(super) mod jwks;
pub(super) mod registration;
pub(super) mod revoke;
pub(super) mod token;
pub(super) mod userinfo;

use axum::{Json, response::IntoResponse};
use http::StatusCode;

pub(super) use self::{
	account::*, auth_issuer::*, auth_metadata::*, authorize::*, complete::*, jwks::*,
	registration::*, revoke::*, token::*, userinfo::*,
};

const OIDC_REQ_ID_LENGTH: usize = 32;

fn oauth_error(
	status: StatusCode,
	error: &str,
	description: &str,
) -> http::Response<axum::body::Body> {
	(
		status,
		Json(serde_json::json!({
			"error": error,
			"error_description": description,
		})),
	)
		.into_response()
}
