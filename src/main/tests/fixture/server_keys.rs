//! Signing keys for the tests whose stub peers sign as a homeserver does.
//!
//! Only a binary declaring it by path compiles it, so the boot fixture's other
//! binaries stay free of its dead code.

use std::time::Duration;

use serde_json::{from_value, json};
use tuwunel_core::{
	Result, err,
	ruma::{
		CanonicalJsonObject, MilliSecondsSinceUnixEpoch, ServerName,
		room_version_rules::RedactionRules,
		serde::Base64,
		signatures::{Ed25519KeyPair, sign_event, sign_json},
	},
	utils::time::timepoint_from_now,
};

/// Generate a fresh Ed25519 key pair under the given key version.
///
/// Its signatures name the key `ed25519:` followed by that version.
pub(crate) fn key_pair(version: &str) -> Result<Ed25519KeyPair> {
	Ed25519KeyPair::from_der(&Ed25519KeyPair::generate(), version.into())
		.map_err(|e| err!("{version} key: {e}"))
}

/// A server's key document, publishing one key and signed with it.
///
/// It stays valid for an hour, longer than any test runs, so the server under
/// test may cache it.
pub(crate) fn key_document(
	name: &ServerName,
	key: &Ed25519KeyPair,
) -> Result<CanonicalJsonObject> {
	let valid_until = timepoint_from_now(Duration::from_hours(1))?;
	let valid_until_ts = MilliSecondsSinceUnixEpoch::from_system_time(valid_until)
		.ok_or_else(|| err!("key document expiry out of range"))?;

	let key_id = format!("ed25519:{}", key.version());
	let public_key: Base64 = Base64::new(key.public_key().to_vec());
	let document = json!({
		"server_name": name,
		"valid_until_ts": valid_until_ts,
		"verify_keys": { key_id: { "key": public_key } },
		"old_verify_keys": {},
	});

	signed(from_value(document)?, name, key)
}

/// Signs an event as the given server, under the given redaction rules.
///
/// The event is otherwise returned unchanged, its content hash included.
pub(crate) fn signed_event(
	mut event: CanonicalJsonObject,
	name: &ServerName,
	key: &Ed25519KeyPair,
	rules: &RedactionRules,
) -> Result<CanonicalJsonObject> {
	sign_event(name.as_str(), key, &mut event, rules)
		.map_err(|e| err!("sign event as {name}: {e}"))?;

	Ok(event)
}

/// Signs a JSON object as the given server.
///
/// The signature joins any the object already carries.
fn signed(
	mut object: CanonicalJsonObject,
	name: &ServerName,
	key: &Ed25519KeyPair,
) -> Result<CanonicalJsonObject> {
	sign_json(name.as_str(), key, &mut object).map_err(|e| err!("sign as {name}: {e}"))?;

	Ok(object)
}
