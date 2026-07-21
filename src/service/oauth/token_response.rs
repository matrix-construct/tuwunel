use serde::Deserialize;

/// Fields deserialized from an upstream provider's `/token` response.
///
/// This separate shape omits `expires_at`: some providers encode it as a Unix
/// timestamp, while the persisted `Session` stores a `SystemTime` derived from
/// `expires_in`.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
	/// Token type (bearer, mac, etc).
	pub token_type: Option<String>,

	/// Access token granted by the provider.
	pub access_token: Option<String>,

	/// Duration in seconds the access_token is valid for.
	pub expires_in: Option<u64>,

	/// Token used to refresh the access_token.
	pub refresh_token: Option<String>,

	/// Duration in seconds the refresh_token is valid for.
	pub refresh_token_expires_in: Option<u64>,

	/// Access scope actually granted (if supported).
	pub scope: Option<String>,

	/// Signed JWT containing the user's identity claims (OIDC).
	pub id_token: Option<String>,
}
