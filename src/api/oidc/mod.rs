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

use std::fmt::Write;

use axum::{Json, body::Body, response::IntoResponse};
use http::{Response, StatusCode};
use serde_json::json;

pub(super) use self::{
	account::*, auth_issuer::*, auth_metadata::*, authorize::*, complete::*, jwks::*,
	registration::*, revoke::*, token::*, userinfo::*,
};

const OIDC_REQ_ID_LENGTH: usize = 32;

pub(crate) fn url_encode(s: &str) -> String {
	s.bytes()
		.fold(String::with_capacity(s.len()), |mut out, b| {
			if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
				out.push(b.into());
			} else {
				write!(&mut out, "%{b:02X}").ok();
			}

			out
		})
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response<Body> {
	let body = json!({
		"error": error,
		"error_description": description,
	});

	(status, Json(body)).into_response()
}
