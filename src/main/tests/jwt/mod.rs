//! Signs the JWTs a test server accepts under its configured shared secret.

use serde_json::json;
use tuwunel_core::{
	Result, err,
	jwt::{EncodingKey, Header, encode},
	ruma::{UserId, exports::serde::Serialize},
};

/// Mint a token naming the user's localpart as its subject.
pub(crate) fn mint(secret: &str, user_id: &UserId) -> Result<String> {
	sign(secret, &json!({"sub": user_id.localpart()}))
}

/// Sign claims with HS256 under a shared secret.
///
/// The secret is the one the test configures on the server under test, so the
/// server accepts the token.
pub(crate) fn sign<Claims>(secret: &str, claims: &Claims) -> Result<String>
where
	Claims: Serialize,
{
	EncodingKey::from_secret(secret.as_bytes())
		.and_then(|key| encode(&Header::default(), claims, &key))
		.map_err(|error| err!("failed to sign a JWT: {error}"))
}
