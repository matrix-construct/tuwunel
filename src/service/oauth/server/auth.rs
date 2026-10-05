use std::time::{Duration, SystemTime};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use ruma::OwnedUserId;
use serde::{Deserialize, Serialize};
use tuwunel_core::{Err, Result, err, implement, utils, utils::hash::sha256};
use tuwunel_database::{Cbor, Deserialized};

use super::AuthCode;

/// A pending authorization request, kept from the authorize redirect until the
/// completion that mints its code.
///
/// Native sign-in binds it to one upstream provider or claims it for the local
/// branch, and completion takes it exactly once.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuthRequest {
	pub client_id: String,

	pub redirect_uri: String,

	pub scope: String,

	pub state: Option<String>,

	pub nonce: Option<String>,

	pub code_challenge: Option<String>,

	pub code_challenge_method: Option<String>,

	/// The identity provider ID used to authenticate the user for this
	/// authorization request. Stored so it can be propagated to the device
	/// at token exchange time and used for UIAA SSO provider binding.
	pub idp_id: Option<String>,

	/// Whether a local login or registration has claimed this request.
	///
	/// A claim excludes provider selection, as `idp_id` excludes the local
	/// branch.
	#[serde(default)]
	pub local_auth_selected: bool,

	pub response_mode: Option<String>,

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

	/// Propagated from the originating AuthRequest; identifies which IdP
	/// authenticated the user so the device can be tagged at token exchange.
	pub idp_id: Option<String>,

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
		idp_id: auth_req.idp_id.clone(),
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

/// Read an authorization request without consuming it.
///
/// A flow that pauses for a user gesture reads the request to decide what to
/// show, then takes it with `take_auth_request` or retires it with
/// `retire_auth_request` once the gesture arrives. An unknown or expired request
/// is a `NotFound`, and an expired one is evicted as it is found, so no caller
/// ever renders against a stale request.
#[implement(super::Server)]
pub async fn peek_auth_request(&self, req_id: &str) -> Result<AuthRequest> {
	let request = self
		.db
		.oidcreqid_authrequest
		.get(req_id)
		.await
		.deserialized()
		.map(|cbor: Cbor<AuthRequest>| cbor.0)
		.map_err(|_| err!(Request(NotFound("Unknown or expired authorization request"))))?;

	if SystemTime::now() > request.expires_at {
		self.remove_auth_request(req_id);

		return Err!(Request(NotFound("Authorization request has expired")));
	}

	Ok(request)
}

/// Bind a native authorization request to one selected upstream provider.
///
/// A different provider or a local claim is refused, so one request cannot
/// complete through two branches. Choosing the same provider again is
/// harmless, so a repeated click still redirects.
#[implement(super::Server)]
pub async fn bind_auth_request_to_provider(&self, req_id: &str, provider_id: &str) -> Result {
	self.update_auth_request(req_id, |request| {
		no_other_provider(request, Some(provider_id))
			.and_then(local_unclaimed)
			.map(|request| AuthRequest {
				idp_id: Some(provider_id.to_owned()),
				..request
			})
	})
	.await
}

/// Check that a pending request may still complete through the local branch.
///
/// A request bound to an upstream provider is refused. One the local branch
/// already claimed passes, so a resubmitted form still completes.
#[implement(super::Server)]
pub async fn check_local_auth_request(&self, req_id: &str) -> Result {
	self.peek_auth_request(req_id)
		.await
		.and_then(|request| no_other_provider(request, None))
		.map(drop)
}

/// Claim the local branch of a native authorization request.
///
/// A request already bound to a provider is refused. Repeating the claim is
/// harmless, so a resubmitted form still completes, and the request stays
/// single-use when it is taken.
#[implement(super::Server)]
pub async fn bind_auth_request_to_local(&self, req_id: &str) -> Result {
	self.update_auth_request(req_id, |request| {
		no_other_provider(request, None)
			.map(|request| AuthRequest { local_auth_selected: true, ..request })
	})
	.await
}

/// Take a pending request exactly once, removing it.
///
/// A request that changed since the caller read it is refused. The lock is the
/// one provider selection and local claims acquire, so neither can change the
/// request between the comparison and the removal.
#[implement(super::Server)]
pub async fn take_auth_request(
	&self,
	req_id: &str,
	expected: &AuthRequest,
) -> Result<AuthRequest> {
	let _lock = self.auth_request_locks.lock(req_id).await;
	let request = self.peek_auth_request(req_id).await?;

	if &request != expected {
		return Err!(Request(Forbidden("Authorization request changed during sign-in")));
	}

	self.remove_auth_request(req_id);

	Ok(request)
}

/// Retire a pending request under its lock without reading it.
///
/// A refusal needs nothing from the request, and holding the lock keeps a
/// concurrent provider selection from writing it back.
#[implement(super::Server)]
pub async fn retire_auth_request(&self, req_id: &str) {
	let _lock = self.auth_request_locks.lock(req_id).await;

	self.remove_auth_request(req_id);
}

/// Remove a pending authorization request without taking its lock.
///
/// The request is single-use, so a flow removes it before minting anything
/// against it. Removing a key that is already gone is a no-op.
#[implement(super::Server)]
pub fn remove_auth_request(&self, req_id: &str) { self.db.oidcreqid_authrequest.remove(req_id); }

/// Consumes an authorization code at most once within this server.
///
/// Reading and removing a code are serialized independently of authorization
/// request selection. A decoded code is removed before validating its expiry,
/// client, redirect URI, or PKCE verifier.
#[implement(super::Server)]
pub async fn exchange_auth_code(
	&self,
	code: &str,
	client_id: &str,
	redirect_uri: &str,
	code_verifier: Option<&str>,
	require_pkce: bool,
) -> Result<AuthCodeSession> {
	let key = AuthCode(code.to_owned());
	let lock = self.auth_code_locks.lock(&key).await;
	let session: AuthCodeSession = self
		.db
		.oidccode_authsession
		.get(code)
		.await
		.deserialized::<Cbor<_>>()
		.map(|cbor: Cbor<AuthCodeSession>| cbor.0)
		.map_err(|_| err!(Request(Forbidden("Invalid or expired authorization code"))))?;

	self.db.oidccode_authsession.remove(code);
	drop(lock);

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
		// Reject a challenge-less code when PKCE is required: the knob is
		// reloadable and codes outlive an off->on flip of it.
		if require_pkce {
			return Err!(Request(Forbidden(
				"the authorization request carried no PKCE code_challenge"
			)));
		}

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

	// Only S256 is advertised in discovery metadata; reject plain to avoid
	// downgrade attacks (plain challenge == verifier, trivially intercepted).
	let computed = match method {
		| "S256" => b64.encode(sha256::hash(verifier.as_bytes())),
		| _ => return Err!(Request(InvalidParam("Unsupported code_challenge_method"))),
	};

	if computed != *challenge {
		return Err!(Request(Forbidden("PKCE verification failed")));
	}

	Ok(session)
}

/// Rewrite a pending request under its lock.
///
/// A selection decided against one read cannot then be lost to a concurrent
/// one.
#[implement(super::Server)]
async fn update_auth_request<F>(&self, req_id: &str, update: F) -> Result
where
	F: FnOnce(AuthRequest) -> Result<AuthRequest> + Send,
{
	let _lock = self.auth_request_locks.lock(req_id).await;
	let request = self
		.peek_auth_request(req_id)
		.await
		.and_then(update)?;

	self.store_auth_request(req_id, &request);

	Ok(())
}

/// Refuse a request bound to any provider other than `provider`.
///
/// The local branch passes `None`, so every provider binding refuses it.
fn no_other_provider(request: AuthRequest, provider: Option<&str>) -> Result<AuthRequest> {
	if request
		.idp_id
		.as_deref()
		.is_some_and(|bound| Some(bound) != provider)
	{
		return Err!(Request(Forbidden("Authorization request already selected a provider")));
	}

	Ok(request)
}

/// Refuse a request the local branch has already claimed.
///
/// A claim excludes provider selection but not a repeated local claim.
fn local_unclaimed(request: AuthRequest) -> Result<AuthRequest> {
	if request.local_auth_selected {
		return Err!(Request(Forbidden(
			"A local sign-in already claimed this authorization request"
		)));
	}

	Ok(request)
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
