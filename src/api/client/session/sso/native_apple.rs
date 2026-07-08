use std::{
	collections::{BTreeMap, BTreeSet},
	fmt::Write as _,
	future::Future,
	sync::OnceLock,
	time::{Duration, Instant},
};

use axum::{Json, extract::State, response::IntoResponse};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::sync::RwLock;
use tuwunel_core::{
	Err, Result, at,
	config::IdentityProvider,
	debug_info, err,
	jwt::{Algorithm, DecodingKey, Header, Validation, decode, decode_header},
	utils,
	utils::hash::sha256,
};
use tuwunel_service::{
	Services,
	oauth::{Provider, SESSION_ID_LENGTH, Session, UserInfo},
};

use super::{super::TOKEN_LENGTH, complete_sso_session};

static APPLE_ISSUER: &str = "https://appleid.apple.com";
static APPLE_JWKS_URL: &str = "https://appleid.apple.com/auth/keys";
static APPLE_JWKS_CACHE: OnceLock<RwLock<Option<CachedAppleJwks>>> = OnceLock::new();

const APPLE_JWKS_CACHE_TTL: Duration = Duration::from_mins(10);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeAppleLoginRequest {
	identity_token: String,
	// Kept for auditability and future code-exchange/revocation checks; the
	// current exchange validates the identity token directly.
	authorization_code: Option<String>,
	nonce: Option<String>,
	provider_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeAppleLoginResponse {
	login_token: String,
	expires_in_ms: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct AppleJwks {
	keys: Vec<AppleJwk>,
}

#[derive(Clone, Debug, Deserialize)]
struct AppleJwk {
	alg: Option<String>,
	e: String,
	kid: String,
	kty: String,
	n: String,
}

#[derive(Debug, Deserialize)]
struct AppleIdTokenClaims {
	iss: String,
	aud: String,
	#[expect(
		dead_code,
		reason = "jsonwebtoken validates this registered claim"
	)]
	exp: u64,
	sub: String,
	email: Option<String>,
	name: Option<String>,
	given_name: Option<String>,
	family_name: Option<String>,
	nonce: Option<String>,
}

#[derive(Clone, Debug)]
struct CachedAppleJwks {
	jwks: AppleJwks,
	fetched_at: Instant,
}

fn apple_native_audiences(provider: &Provider) -> BTreeSet<String> {
	let mut audiences = provider.native_client_ids.clone();
	audiences.insert(provider.client_id.clone());
	audiences
}

fn sha256_hex(value: &str) -> String {
	let digest = sha256::hash(value.as_bytes());
	let mut output = String::new();

	for byte in digest {
		write!(&mut output, "{byte:02x}").expect("write to string");
	}

	output
}

fn apple_userinfo_from_validated_claims(
	provider: &Provider,
	claims: AppleIdTokenClaims,
	raw_nonce: Option<&str>,
) -> Result<UserInfo> {
	if claims.iss != APPLE_ISSUER {
		return Err!(Request(Unauthorized("Apple id_token issuer is not trusted.")));
	}

	if !apple_native_audiences(provider).contains(&claims.aud) {
		return Err!(Request(Unauthorized(
			"Apple id_token audience is not configured for this provider."
		)));
	}

	match (claims.nonce.as_deref(), raw_nonce) {
		| (Some(token_nonce), Some(raw_nonce)) => {
			let expected_nonce = sha256_hex(raw_nonce);
			if token_nonce != expected_nonce.as_str() {
				return Err!(Request(Unauthorized("Apple id_token nonce does not match.")));
			}
		},
		| (Some(_), None) => {
			return Err!(Request(Unauthorized(
				"Apple id_token nonce is present but no request nonce was supplied."
			)));
		},
		| (None, Some(_)) => {
			return Err!(Request(Unauthorized("Apple id_token nonce is missing.")));
		},
		| (None, None) => {},
	}

	Ok(apple_userinfo_from_claim_values(
		claims.sub,
		claims.email,
		claims.name,
		claims.given_name,
		claims.family_name,
	))
}

fn apple_userinfo_from_claim_values(
	sub: String,
	email: Option<String>,
	name: Option<String>,
	given_name: Option<String>,
	family_name: Option<String>,
) -> UserInfo {
	let preferred_username = email
		.as_deref()
		.and_then(|value| value.split_once('@'))
		.map(at!(0))
		.map(ToOwned::to_owned);

	UserInfo {
		sub,
		preferred_username: preferred_username.clone(),
		username: preferred_username,
		nickname: None,
		name,
		given_name,
		family_name,
		email,
		avatar_url: None,
		picture: None,
	}
}

pub(super) fn decode_userinfo_from_id_token(session: &Session) -> Result<UserInfo> {
	let id_token = session.id_token.as_deref().ok_or_else(|| {
		err!(Request(Unauthorized("Missing Apple id_token in token response.")))
	})?;

	let payload_b64 = id_token
		.split('.')
		.nth(1)
		.ok_or_else(|| err!(Request(Unauthorized("Apple id_token is malformed."))))?;

	let payload = b64
		.decode(payload_b64)
		.map_err(|_| err!(Request(Unauthorized("Apple id_token payload is invalid base64."))))?;

	let payload: JsonValue = serde_json::from_slice(&payload)
		.map_err(|_| err!(Request(Unauthorized("Apple id_token payload is not valid JSON."))))?;

	let sub = payload
		.get("sub")
		.and_then(JsonValue::as_str)
		.ok_or_else(|| {
			err!(Request(Unauthorized("Apple id_token missing required sub claim.")))
		})?;

	let email = payload
		.get("email")
		.and_then(JsonValue::as_str)
		.map(ToOwned::to_owned);

	Ok(apple_userinfo_from_claim_values(
		sub.to_owned(),
		email,
		payload
			.get("name")
			.and_then(JsonValue::as_str)
			.map(ToOwned::to_owned),
		payload
			.get("given_name")
			.and_then(JsonValue::as_str)
			.map(ToOwned::to_owned),
		payload
			.get("family_name")
			.and_then(JsonValue::as_str)
			.map(ToOwned::to_owned),
	))
}

fn apple_id_token_header(token: &str) -> Result<Header> {
	let header = decode_header(token)
		.map_err(|e| err!(Request(Unauthorized("Apple id_token header is invalid: {e}"))))?;

	if header.alg != Algorithm::RS256 {
		return Err!(Request(Unauthorized("Apple id_token uses unsupported signing algorithm.")));
	}

	if header.kid.is_none() {
		return Err!(Request(Unauthorized("Apple id_token missing key id.")));
	}

	Ok(header)
}

fn apple_decoding_key_for_kid(kid: &str, jwks: &AppleJwks) -> Result<Option<DecodingKey>> {
	let Some(jwk) = jwks.keys.iter().find(|key| key.kid == kid) else {
		return Ok(None);
	};

	if jwk.kty != "RSA"
		|| jwk
			.alg
			.as_deref()
			.is_some_and(|alg| alg != "RS256")
	{
		return Err!(Request(Unauthorized("Apple id_token key is not an RSA signing key.")));
	}

	DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
		.map(Some)
		.map_err(|e| err!(Request(Unauthorized("Apple id_token signing key is invalid: {e}"))))
}

fn apple_jwks_contains_kid(jwks: &AppleJwks, kid: &str) -> bool {
	jwks.keys.iter().any(|key| key.kid == kid)
}

fn cached_apple_jwks_is_fresh(cached: &CachedAppleJwks) -> bool {
	cached.fetched_at.elapsed() < APPLE_JWKS_CACHE_TTL
}

fn apple_id_token_validation(provider: &Provider) -> Validation {
	let audiences = apple_native_audiences(provider)
		.into_iter()
		.collect::<Vec<_>>();
	let issuers = [APPLE_ISSUER.to_owned()];
	let required_spec_claims: Vec<_> = ["iss", "aud", "exp", "sub"].into();
	let mut validation = Validation::new(Algorithm::RS256);

	validation.set_audience(&audiences);
	validation.set_issuer(&issuers);
	validation.set_required_spec_claims(&required_spec_claims);

	validation
}

fn native_apple_provider_id(
	requested_provider_id: Option<&str>,
	identity_providers: &BTreeMap<String, IdentityProvider>,
) -> Result<String> {
	if let Some(provider_id) = requested_provider_id {
		return Ok(provider_id.to_owned());
	}

	let mut apple_providers = identity_providers
		.values()
		.filter(|provider| provider.brand == "appleoidc")
		.map(IdentityProvider::id);
	let Some(provider_id) = apple_providers.next() else {
		return Err!(Request(NotFound(
			"No AppleOIDC identity provider is configured for native Apple login."
		)));
	};

	if apple_providers.next().is_some() {
		return Err!(Request(InvalidParam(
			"Native Apple login requires provider_id when multiple AppleOIDC identity providers \
			 are configured."
		)));
	}

	Ok(provider_id.to_owned())
}

async fn fetch_apple_jwks(services: &Services) -> Result<AppleJwks> {
	services
		.client
		.oauth
		.get(APPLE_JWKS_URL)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await
		.map_err(Into::into)
}

async fn cached_apple_jwks(services: &Services) -> Result<AppleJwks> {
	let cache = APPLE_JWKS_CACHE.get_or_init(|| RwLock::new(None));

	{
		let cached = cache.read().await;
		if let Some(cached) = cached
			.as_ref()
			.filter(|cached| cached_apple_jwks_is_fresh(cached))
		{
			return Ok(cached.jwks.clone());
		}
	}

	let mut cached = cache.write().await;
	if let Some(cached) = cached
		.as_ref()
		.filter(|cached| cached_apple_jwks_is_fresh(cached))
	{
		return Ok(cached.jwks.clone());
	}

	let jwks = fetch_apple_jwks(services).await?;
	*cached = Some(CachedAppleJwks {
		jwks: jwks.clone(),
		fetched_at: Instant::now(),
	});

	Ok(jwks)
}

async fn refresh_apple_jwks_from_cache(
	cache: &RwLock<Option<CachedAppleJwks>>,
	kid: &str,
	fetch: impl Future<Output = Result<AppleJwks>>,
) -> Result<AppleJwks> {
	let mut cached = cache.write().await;

	if let Some(cached) = cached
		.as_ref()
		.filter(|cached| cached_apple_jwks_is_fresh(cached))
		.filter(|cached| apple_jwks_contains_kid(&cached.jwks, kid))
	{
		return Ok(cached.jwks.clone());
	}

	let jwks = fetch.await?;
	*cached = Some(CachedAppleJwks {
		jwks: jwks.clone(),
		fetched_at: Instant::now(),
	});

	Ok(jwks)
}

async fn refresh_apple_jwks(services: &Services, kid: &str) -> Result<AppleJwks> {
	let cache = APPLE_JWKS_CACHE.get_or_init(|| RwLock::new(None));

	refresh_apple_jwks_from_cache(cache, kid, fetch_apple_jwks(services)).await
}

async fn validate_apple_identity_token(
	services: &Services,
	provider: &Provider,
	identity_token: &str,
) -> Result<AppleIdTokenClaims> {
	let header = apple_id_token_header(identity_token)?;
	let kid = header
		.kid
		.as_deref()
		.expect("apple_id_token_header validates kid");
	let jwks = cached_apple_jwks(services).await?;
	let decoding_key = if let Some(decoding_key) = apple_decoding_key_for_kid(kid, &jwks)? {
		decoding_key
	} else {
		let jwks = refresh_apple_jwks(services, kid).await?;
		apple_decoding_key_for_kid(kid, &jwks)?
			.ok_or_else(|| err!(Request(Unauthorized("Apple id_token key id is not trusted."))))?
	};
	let validation = apple_id_token_validation(provider);

	decode::<AppleIdTokenClaims>(identity_token, &decoding_key, &validation)
		.map(|decoded| decoded.claims)
		.map_err(|e| err!(Request(Unauthorized("Apple id_token is invalid: {e}"))))
}

#[tracing::instrument(name = "native_apple_login", level = "info", skip_all)]
pub(crate) async fn native_apple_login_route(
	State(services): State<crate::State>,
	Json(body): Json<NativeAppleLoginRequest>,
) -> Result<impl IntoResponse> {
	let provider_id = native_apple_provider_id(
		body.provider_id.as_deref(),
		&services.config.identity_provider,
	)?;
	let provider = services.oauth.providers.get(&provider_id).await?;

	if provider.brand != "appleoidc" {
		return Err!(Request(InvalidParam(
			"Native Apple login requires an AppleOIDC identity provider."
		)));
	}

	let claims =
		validate_apple_identity_token(&services, &provider, &body.identity_token).await?;
	let userinfo =
		apple_userinfo_from_validated_claims(&provider, claims, body.nonce.as_deref())?;
	let sess_id = utils::random_string(SESSION_ID_LENGTH);
	let session = Session {
		idp_id: Some(provider.id().to_owned()),
		sess_id: Some(sess_id),
		id_token: Some(body.identity_token),
		..Default::default()
	};

	if let Some(authorization_code) = body.authorization_code.as_deref() {
		debug_info!(
			code_len = authorization_code.len(),
			provider = provider.id(),
			"Received native Apple authorization code alongside id_token.",
		);
	}

	let (user_id, _) = complete_sso_session(&services, &provider, session, userinfo).await?;
	let login_token = utils::random_string(TOKEN_LENGTH);
	let expires_in_ms = services
		.users
		.create_login_token(&user_id, &login_token);

	Ok((StatusCode::OK, Json(NativeAppleLoginResponse { login_token, expires_in_ms })))
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering};

	use serde_json::json;

	use super::*;

	fn apple_provider_with_native_clients(native_client_ids: &[&str]) -> Provider {
		Provider {
			brand: "appleoidc".to_owned(),
			client_id: "chat.mindroom.matrix.apple".to_owned(),
			client_secret: None,
			client_secret_file: None,
			issuer_url: Some(
				"https://appleid.apple.com"
					.parse()
					.expect("issuer URL"),
			),
			callback_url: None,
			default: false,
			name: Some("Apple".to_owned()),
			icon: None,
			scope: BTreeSet::new(),
			userid_claims: BTreeSet::new(),
			trusted: false,
			unique_id_fallbacks: true,
			registration: true,
			base_path: None,
			discovery_url: None,
			authorization_url: None,
			token_url: None,
			revocation_url: None,
			introspection_url: None,
			userinfo_url: None,
			discovery: true,
			grant_session_duration: Some(300),
			check_cookie: true,
			forward_action_prompt: false,
			extra_authorization_parameters: BTreeMap::new(),
			native_client_ids: native_client_ids
				.iter()
				.map(ToString::to_string)
				.collect::<BTreeSet<_>>(),
		}
	}

	fn apple_claims(audience: &str) -> AppleIdTokenClaims {
		AppleIdTokenClaims {
			iss: "https://appleid.apple.com".to_owned(),
			aud: audience.to_owned(),
			exp: 4_102_444_800,
			sub: "apple-user-123".to_owned(),
			email: Some("alice@example.com".to_owned()),
			name: None,
			given_name: None,
			family_name: None,
			nonce: Some(sha256_hex("native-nonce")),
		}
	}

	fn apple_session_with_claims(claims: &serde_json::Value) -> Session {
		let payload = b64.encode(serde_json::to_vec(claims).expect("serialize claims"));

		Session {
			id_token: Some(format!("header.{payload}.signature")),
			..Default::default()
		}
	}

	fn apple_test_jwk(kid: &str) -> AppleJwk {
		AppleJwk {
			alg: Some("RS256".to_owned()),
			e: "AQAB".to_owned(),
			kid: kid.to_owned(),
			kty: "RSA".to_owned(),
			n: "yRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5_CYYi_cvI-SXVT9kPWSKXxJXBXd_4LkvcPuUakBoAkfh-eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG_AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi-yUod-j8MtvIj812dkS4QMiRVN_by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQ".to_owned(),
		}
	}

	fn apple_test_jwks(kids: &[&str]) -> AppleJwks {
		AppleJwks {
			keys: kids
				.iter()
				.map(|kid| apple_test_jwk(kid))
				.collect(),
		}
	}

	#[test]
	fn apple_decoding_key_lookup_allows_refresh_when_kid_is_unknown() {
		let cached_jwks = apple_test_jwks(&["cached-key"]);
		let fresh_jwks = apple_test_jwks(&["rotated-key"]);

		assert!(
			apple_decoding_key_for_kid("rotated-key", &cached_jwks)
				.expect("unknown kid should not be a hard failure before refresh")
				.is_none()
		);
		assert!(
			apple_decoding_key_for_kid("rotated-key", &fresh_jwks)
				.expect("refreshed JWKS should resolve rotated kid")
				.is_some()
		);
	}

	#[tokio::test]
	async fn refresh_apple_jwks_reuses_cache_when_waited_refresh_contains_kid() {
		let cache = RwLock::new(Some(CachedAppleJwks {
			jwks: apple_test_jwks(&["rotated-key"]),
			fetched_at: Instant::now(),
		}));
		let fetches = AtomicUsize::new(0);

		let jwks = refresh_apple_jwks_from_cache(&cache, "rotated-key", async {
			fetches.fetch_add(1, Ordering::SeqCst);
			Ok(apple_test_jwks(&["unused-network-key"]))
		})
		.await
		.expect("cached key should be reused without fetching");

		assert!(apple_jwks_contains_kid(&jwks, "rotated-key"));
		assert_eq!(fetches.load(Ordering::SeqCst), 0);
	}

	#[tokio::test]
	async fn refresh_apple_jwks_fetches_when_locked_cache_still_misses_kid() {
		let cache = RwLock::new(Some(CachedAppleJwks {
			jwks: apple_test_jwks(&["cached-key"]),
			fetched_at: Instant::now(),
		}));
		let fetches = AtomicUsize::new(0);

		let jwks = refresh_apple_jwks_from_cache(&cache, "rotated-key", async {
			fetches.fetch_add(1, Ordering::SeqCst);
			Ok(apple_test_jwks(&["rotated-key"]))
		})
		.await
		.expect("missing key should trigger one refresh");

		assert!(apple_jwks_contains_kid(&jwks, "rotated-key"));
		assert_eq!(fetches.load(Ordering::SeqCst), 1);

		let cached = cache.read().await;
		let cached = cached
			.as_ref()
			.expect("refreshed JWKS should be cached");
		assert!(apple_jwks_contains_kid(&cached.jwks, "rotated-key"));
	}

	#[test]
	fn decode_userinfo_from_id_token_extracts_expected_claims() {
		let session = apple_session_with_claims(&json!({
			"sub": "apple-user-123",
			"email": "alice@example.com",
			"name": "Alice Example",
			"given_name": "Alice",
			"family_name": "Example"
		}));

		let userinfo =
			decode_userinfo_from_id_token(&session).expect("decode Apple id_token claims");

		assert_eq!(userinfo.sub, "apple-user-123");
		assert_eq!(userinfo.email.as_deref(), Some("alice@example.com"));
		assert_eq!(userinfo.preferred_username.as_deref(), Some("alice"));
		assert_eq!(userinfo.username.as_deref(), Some("alice"));
		assert_eq!(userinfo.name.as_deref(), Some("Alice Example"));
		assert_eq!(userinfo.given_name.as_deref(), Some("Alice"));
		assert_eq!(userinfo.family_name.as_deref(), Some("Example"));
	}

	#[test]
	fn decode_userinfo_from_id_token_requires_sub_claim() {
		let session = apple_session_with_claims(&json!({
			"email": "alice@example.com"
		}));

		let error =
			decode_userinfo_from_id_token(&session).expect_err("missing sub claim should fail");

		let message = format!("{error}");
		assert!(message.contains("sub claim"), "unexpected error: {message}");
	}

	#[test]
	fn decode_userinfo_from_id_token_requires_id_token() {
		let session = Session::default();

		let error =
			decode_userinfo_from_id_token(&session).expect_err("missing id_token should fail");

		let message = format!("{error}");
		assert!(message.contains("Missing Apple id_token"), "unexpected error: {message}");
	}

	#[test]
	fn decode_userinfo_from_id_token_rejects_invalid_payload() {
		let session = Session {
			id_token: Some("header.!.signature".to_owned()),
			..Default::default()
		};

		let error = decode_userinfo_from_id_token(&session)
			.expect_err("invalid id_token payload should fail");

		let message = format!("{error}");
		assert!(message.contains("invalid base64"), "unexpected error: {message}");
	}

	#[test]
	fn native_apple_claims_accept_configured_bundle_audience() {
		let provider = apple_provider_with_native_clients(&["chat.mindroom.app"]);
		let claims = apple_claims("chat.mindroom.app");

		let userinfo =
			apple_userinfo_from_validated_claims(&provider, claims, Some("native-nonce"))
				.expect("configured native bundle audience should be accepted");

		assert_eq!(userinfo.sub, "apple-user-123");
		assert_eq!(userinfo.email.as_deref(), Some("alice@example.com"));
		assert_eq!(userinfo.preferred_username.as_deref(), Some("alice"));
	}

	#[test]
	fn native_apple_claims_accept_web_services_audience_for_compatibility() {
		let provider = apple_provider_with_native_clients(&[]);
		let claims = apple_claims("chat.mindroom.matrix.apple");

		apple_userinfo_from_validated_claims(&provider, claims, Some("native-nonce"))
			.expect("provider client_id audience should remain accepted");
	}

	#[test]
	fn native_apple_claims_reject_unconfigured_audience() {
		let provider = apple_provider_with_native_clients(&[]);
		let claims = apple_claims("chat.mindroom.app");

		let error = apple_userinfo_from_validated_claims(&provider, claims, Some("native-nonce"))
			.expect_err("unconfigured native bundle audience should be rejected");

		let message = format!("{error}");
		assert!(message.contains("audience"), "unexpected error: {message}");
	}

	#[test]
	fn native_apple_claims_reject_wrong_issuer() {
		let provider = apple_provider_with_native_clients(&["chat.mindroom.app"]);
		let mut claims = apple_claims("chat.mindroom.app");
		claims.iss = "https://example.com".to_owned();

		let error = apple_userinfo_from_validated_claims(&provider, claims, Some("native-nonce"))
			.expect_err("wrong issuer should be rejected");

		let message = format!("{error}");
		assert!(message.contains("issuer"), "unexpected error: {message}");
	}

	#[test]
	fn native_apple_claims_reject_nonce_mismatch() {
		let provider = apple_provider_with_native_clients(&["chat.mindroom.app"]);
		let claims = apple_claims("chat.mindroom.app");

		let error =
			apple_userinfo_from_validated_claims(&provider, claims, Some("different-nonce"))
				.expect_err("nonce mismatch should be rejected");

		let message = format!("{error}");
		assert!(message.contains("nonce"), "unexpected error: {message}");
	}

	#[test]
	fn native_apple_claims_reject_token_nonce_without_request_nonce() {
		let provider = apple_provider_with_native_clients(&["chat.mindroom.app"]);
		let claims = apple_claims("chat.mindroom.app");

		let error = apple_userinfo_from_validated_claims(&provider, claims, None)
			.expect_err("token nonce without request nonce should be rejected");

		let message = format!("{error}");
		assert!(message.contains("nonce"), "unexpected error: {message}");
	}

	#[test]
	fn native_apple_claims_reject_request_nonce_without_token_nonce() {
		let provider = apple_provider_with_native_clients(&["chat.mindroom.app"]);
		let mut claims = apple_claims("chat.mindroom.app");
		claims.nonce = None;

		let error = apple_userinfo_from_validated_claims(&provider, claims, Some("native-nonce"))
			.expect_err("request nonce without token nonce should be rejected");

		let message = format!("{error}");
		assert!(message.contains("nonce"), "unexpected error: {message}");
	}

	#[test]
	fn native_apple_provider_id_uses_explicit_provider_id() {
		let providers =
			[("apple".to_owned(), apple_provider_with_native_clients(&["chat.mindroom.app"]))]
				.into();

		assert_eq!(
			native_apple_provider_id(Some("chat.mindroom.matrix.apple"), &providers)
				.expect("explicit provider id should pass through"),
			"chat.mindroom.matrix.apple"
		);
	}

	#[test]
	fn native_apple_provider_id_falls_back_to_single_apple_provider() {
		let providers =
			[("apple".to_owned(), apple_provider_with_native_clients(&["chat.mindroom.app"]))]
				.into();

		assert_eq!(
			native_apple_provider_id(None, &providers)
				.expect("single Apple provider should be selected"),
			"chat.mindroom.matrix.apple"
		);
	}

	#[test]
	fn native_apple_provider_id_rejects_missing_apple_provider() {
		let mut provider = apple_provider_with_native_clients(&["chat.mindroom.app"]);
		provider.brand = "google".to_owned();
		let providers = [("google".to_owned(), provider)].into();

		let error = native_apple_provider_id(None, &providers)
			.expect_err("missing Apple provider should fail");

		let message = format!("{error}");
		assert!(message.contains("AppleOIDC"), "unexpected error: {message}");
	}

	#[test]
	fn native_apple_provider_id_rejects_ambiguous_apple_providers() {
		let mut second_provider = apple_provider_with_native_clients(&["chat.mindroom.dev"]);
		second_provider.client_id = "chat.mindroom.matrix.apple.dev".to_owned();
		let providers = [
			("apple".to_owned(), apple_provider_with_native_clients(&["chat.mindroom.app"])),
			("apple-dev".to_owned(), second_provider),
		]
		.into();

		let error = native_apple_provider_id(None, &providers)
			.expect_err("multiple Apple providers should require provider_id");

		let message = format!("{error}");
		assert!(message.contains("provider_id"), "unexpected error: {message}");
	}
}
