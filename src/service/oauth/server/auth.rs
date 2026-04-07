use std::time::{Duration, SystemTime};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use ruma::OwnedUserId;
use serde::{Deserialize, Serialize};
use tuwunel_core::{Err, Result, err, implement, utils};
use tuwunel_database::{Cbor, Deserialized};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AuthRequest {
	pub client_id: String,
	pub redirect_uri: String,
	pub scope: String,
	pub state: Option<String>,
	pub nonce: Option<String>,
	pub code_challenge: Option<String>,
	pub code_challenge_method: Option<String>,
	pub created_at: SystemTime,
	pub expires_at: SystemTime,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AuthCodeSession {
	pub code: String,
	pub client_id: String,
	pub redirect_uri: String,
	pub scope: String,
	pub state: Option<String>,
	pub nonce: Option<String>,
	pub code_challenge: Option<String>,
	pub code_challenge_method: Option<String>,
	pub user_id: OwnedUserId,
	pub created_at: SystemTime,
	pub expires_at: SystemTime,
}

pub const AUTH_REQUEST_LIFETIME: Duration = Duration::from_mins(10);
const AUTH_CODE_LIFETIME: Duration = Duration::from_mins(10);
const AUTH_CODE_LENGTH: usize = 64;

#[implement(super::Server)]
#[must_use]
pub fn create_auth_code(&self, auth_req: &AuthRequest, user_id: OwnedUserId) -> String {
	let now = SystemTime::now();
	let code = utils::random_string(AUTH_CODE_LENGTH);
	let session = AuthCodeSession {
		code: code.clone(),
		client_id: auth_req.client_id.clone(),
		redirect_uri: auth_req.redirect_uri.clone(),
		scope: auth_req.scope.clone(),
		state: auth_req.state.clone(),
		nonce: auth_req.nonce.clone(),
		code_challenge: auth_req.code_challenge.clone(),
		code_challenge_method: auth_req.code_challenge_method.clone(),
		user_id,
		created_at: now,
		expires_at: now.checked_add(AUTH_CODE_LIFETIME).unwrap_or(now),
	};

	self.db
		.oidccode_authsession
		.raw_put(&*code, Cbor(&session));

	code
}

#[implement(super::Server)]
pub fn store_auth_request(&self, req_id: &str, request: &AuthRequest) {
	self.db
		.oidcreqid_authrequest
		.raw_put(req_id, Cbor(request));
}

#[implement(super::Server)]
pub async fn take_auth_request(&self, req_id: &str) -> Result<AuthRequest> {
	let request: AuthRequest = self
		.db
		.oidcreqid_authrequest
		.get(req_id)
		.await
		.deserialized::<Cbor<_>>()
		.map(|cbor: Cbor<AuthRequest>| cbor.0)
		.map_err(|_| err!(Request(NotFound("Unknown or expired authorization request"))))?;

	self.db.oidcreqid_authrequest.remove(req_id);

	if SystemTime::now() > request.expires_at {
		return Err!(Request(NotFound("Authorization request has expired")));
	}

	Ok(request)
}

#[implement(super::Server)]
pub async fn exchange_auth_code(
	&self,
	code: &str,
	client_id: &str,
	redirect_uri: &str,
	code_verifier: Option<&str>,
) -> Result<AuthCodeSession> {
	let session: AuthCodeSession = self
		.db
		.oidccode_authsession
		.get(code)
		.await
		.deserialized::<Cbor<_>>()
		.map(|cbor: Cbor<AuthCodeSession>| cbor.0)
		.map_err(|_| err!(Request(Forbidden("Invalid or expired authorization code"))))?;

	self.db.oidccode_authsession.remove(code);

	if SystemTime::now() > session.expires_at {
		return Err!(Request(Forbidden("Authorization code has expired")));
	}
	if session.client_id != client_id {
		return Err!(Request(Forbidden("client_id mismatch")));
	}
	if session.redirect_uri != redirect_uri {
		return Err!(Request(Forbidden("redirect_uri mismatch")));
	}

	let Some(challenge) = &session.code_challenge else {
		return Ok(session);
	};

	let Some(verifier) = code_verifier else {
		return Err!(Request(Forbidden("code_verifier required for PKCE")));
	};

	validate_code_verifier(verifier)?;

	let method = session
		.code_challenge_method
		.as_deref()
		.unwrap_or("S256");

	let computed = match method {
		| "plain" => verifier.to_owned(),
		| "S256" => {
			let hash = utils::hash::sha256::hash(verifier.as_bytes());
			b64.encode(hash)
		},
		| _ => return Err!(Request(InvalidParam("Unsupported code_challenge_method"))),
	};

	if computed != *challenge {
		return Err!(Request(Forbidden("PKCE verification failed")));
	}

	Ok(session)
}

/// Validate code_verifier per RFC 7636 Section 4.1: must be 43-128
/// characters using only unreserved characters [A-Z] / [a-z] / [0-9] /
/// "-" / "." / "_" / "~".
fn validate_code_verifier(verifier: &str) -> Result {
	if !(43..=128).contains(&verifier.len()) {
		return Err!(Request(InvalidParam("code_verifier must be 43-128 characters")));
	}

	if !verifier
		.bytes()
		.all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b'~')
	{
		return Err!(Request(InvalidParam("code_verifier contains invalid characters")));
	}

	Ok(())
}
