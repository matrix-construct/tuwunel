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

pub(crate) fn url_encode(s: &str) -> String {
	s.bytes().fold(String::with_capacity(s.len()), |mut out, b| {
		if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
			out.push(b as char);
		} else {
			out.push_str(&format!("%{b:02X}"));
		}
		out
	})
}
