use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use jwt::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, err, implement, jwt, utils};
use utils::hash::sha256;

#[derive(Debug, Deserialize, Serialize)]
pub struct IdTokenClaims {
	pub iss: String,
	pub sub: String,
	pub aud: String,
	pub exp: u64,
	pub iat: u64,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub nonce: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub at_hash: Option<String>,
}

#[implement(super::Server)]
pub fn sign_id_token(&self, claims: &IdTokenClaims) -> Result<String> {
	let mut header = Header::new(Algorithm::ES256);
	header.kid = Some(self.key.key_id.clone());

	let key = EncodingKey::from_ec_der(&self.key.key_der);
	jwt::encode(&header, claims, &key).map_err(|e| err!(error!("Failed to sign ID token: {e}")))
}

#[implement(super::Server)]
#[must_use]
#[inline]
pub fn at_hash(access_token: &str) -> String {
	let hash = sha256::hash(access_token.as_bytes());

	b64.encode(&hash[..16])
}
