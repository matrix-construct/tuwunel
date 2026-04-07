use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use ring::{
	rand::SystemRandom,
	signature::{self, EcdsaKeyPair, KeyPair},
};
use serde_json::{Value as JsonValue, json};
use tuwunel_core::{Result, err};

impl super::Server {
	#[inline]
	#[must_use]
	pub fn jwks(&self) -> JsonValue {
		json!({
			"keys": [self.jwk.clone()],
		})
	}
}

pub(super) fn init_jwk(key_der: &[u8], key_id: &str) -> Result<JsonValue> {
	let rng = SystemRandom::new();
	let alg = &signature::ECDSA_P256_SHA256_FIXED_SIGNING;
	let key_pair = EcdsaKeyPair::from_pkcs8(alg, key_der, &rng)
		.map_err(|e| err!(error!("Failed to load ECDSA key: {e}")))?;

	let public_bytes = key_pair.public_key().as_ref();

	Ok(json!({
		"kty": "EC",
		"crv": "P-256",
		"use": "sig",
		"alg": "ES256",
		"kid": key_id,
		"x": b64.encode(&public_bytes[1..33]),
		"y": b64.encode(&public_bytes[33..65]),
	}))
}
