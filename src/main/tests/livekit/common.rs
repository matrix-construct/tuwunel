//! The LiveKit boot tests share a key pair, a call slot and MSC4195's
//! derivations.
//!
//! The derivations are computed here, apart from the server's own code, so a
//! server deriving the wrong room or identity fails the tests.

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use reqwest::Response;
use serde_json::Value;
use tuwunel_core::{
	Result, err,
	jwt::{Algorithm, DecodingKey, Validation, decode},
	utils::hash::sha256::hash as sha256,
};

use super::client::field;

pub(crate) const API_KEY: &str = "livekit-boot-key";
pub(crate) const SECRET: &str = "livekit-boot-secret";

// Element Call's spelling of the call slot, which shares a room with MSC4196's.
pub(crate) const SLOT: &str = "m.call#ROOM";
pub(crate) const FOLDED_SLOT: &str = "m.call#room";

pub(crate) fn claims(jwt: &str) -> Result<Value> {
	let key = DecodingKey::from_secret(SECRET.as_bytes());
	let token = decode(jwt, &key, &Validation::new(Algorithm::HS256))
		.map_err(|e| err!("the token does not verify with the API secret: {e}"))?;

	Ok(token.claims)
}

// MSC4195's derivation: unpadded base64 of SHA-256 over a two-string JSON array.
pub(crate) fn derive(first: &str, second: &str) -> String {
	let json = serde_json::to_vec(&[first, second]).expect("two strings serialize");

	STANDARD_NO_PAD.encode(sha256(json))
}

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn refusal(response: Response) -> Result<(u16, String)> {
	let status = response.status().as_u16();
	let body: Value = response.json().await?;
	let errcode = field(&body, "errcode")?.to_owned();

	Ok((status, errcode))
}

pub(crate) fn with(request: &Value, key: &str, value: impl Into<Value>) -> Value {
	let mut changed = request.clone();

	changed[key] = value.into();
	changed
}
