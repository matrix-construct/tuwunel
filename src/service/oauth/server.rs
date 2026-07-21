mod auth;
mod client;
mod device;
mod jwk;
mod signing_key;
mod token;

use std::sync::Arc;

use serde_json::Value as JsonValue;
use tuwunel_core::{Err, Result, debug_info, debug_warn, err, implement, utils::MutexMap, warn};
use tuwunel_database::Map;

pub use self::{
	auth::{AUTH_REQUEST_LIFETIME, AuthCodeSession, AuthRequest},
	client::{ClientRegistration, DcrRequest},
	device::{
		ApprovedDeviceGrant, DEVICE_GRANT_INTERVAL_SECS, DEVICE_GRANT_LIFETIME, DeviceGrant,
		DeviceGrantPoll, DeviceGrantStatus, format_user_code,
	},
	token::IdTokenClaims,
};
use self::{
	jwk::init_jwk,
	signing_key::{SigningKey, init_signing_key},
};
use crate::services::OnceServices;

pub struct Server {
	services: Arc<OnceServices>,
	db: Data,
	jwk: JsonValue,
	key: SigningKey,

	/// Serializes the read-check-consume of a device grant by its `device_code`
	/// so concurrent polls of one approved grant cannot each mint a device.
	device_locks: MutexMap<String, ()>,
}

struct Data {
	oidc_signingkey: Arc<Map>,
	oidcclientid_registration: Arc<Map>,
	oidccode_authsession: Arc<Map>,
	oidcdevicecode_devicegrant: Arc<Map>,
	oidcusercode_devicecode: Arc<Map>,
	oidcreqid_authrequest: Arc<Map>,
}

impl Server {
	pub(super) fn build(args: &crate::Args<'_>) -> Result<Option<Self>> {
		if !Self::can_build(args) {
			return Ok(None);
		}

		let db = Data {
			oidc_signingkey: args.db["oidc_signingkey"].clone(),
			oidcclientid_registration: args.db["oidcclientid_registration"].clone(),
			oidccode_authsession: args.db["oidccode_authsession"].clone(),
			oidcdevicecode_devicegrant: args.db["oidcdevicecode_devicegrant"].clone(),
			oidcusercode_devicecode: args.db["oidcusercode_devicecode"].clone(),
			oidcreqid_authrequest: args.db["oidcreqid_authrequest"].clone(),
		};

		let key = init_signing_key(&db)?;
		debug_info!(
			key = ?key.key_id,
			"Initializing OIDC server for next-gen auth (MSC2965)"
		);

		Ok(Some(Self {
			services: args.services.clone(),
			db,
			jwk: init_jwk(&key.key_der, &key.key_id)?,
			key,
			device_locks: MutexMap::new(),
		}))
	}
}

#[implement(Server)]
fn can_build(args: &crate::Args<'_>) -> bool {
	let has_idp = !args.server.config.identity_provider.is_empty();
	let has_cwk = args.server.config.well_known.client.is_some();
	let native = args.server.config.oidc_native_auth;

	if (has_idp || native) && !has_cwk {
		warn!("OIDC server (Next-gen auth) requires `well_known.client` to be configured.");

		return false;
	}

	if !has_idp && !native {
		debug_warn!(
			"OIDC server (Next-gen auth) requires at least one `identity_provider`, or \
			 `oidc_native_auth` to be enabled."
		);

		return false;
	}

	true
}

#[implement(Server)]
pub fn issuer_url(&self) -> Result<String> {
	self.services
		.config
		.well_known
		.client
		.as_ref()
		.map(|url| {
			let s = url.to_string();

			if s.ends_with('/') { s } else { format!("{s}/") }
		})
		.ok_or_else(|| {
			err!(Config("well_known.client", "well_known.client must be set for OIDC server"))
		})
}

/// MSC2967 device-scope prefixes, stable spelling first.
const DEVICE_SCOPE_PREFIXES: [&str; 2] =
	["urn:matrix:client:device:", "urn:matrix:org.matrix.msc2967.client:device:"];

/// MSC2967 API-scope prefixes, stable spelling first.
const API_SCOPE_PREFIXES: [&str; 2] =
	["urn:matrix:client:api:", "urn:matrix:org.matrix.msc2967.client:api:"];

/// Restricts a requested OAuth scope to supported tokens per RFC 6749 §3.3.
///
/// Recognized tokens retain their request order, with an MSC2967 device ID
/// returned separately when present. Unknown tokens are dropped unless `strict`
/// is set. Multiple device scopes, empty device IDs, and IDs outside the RFC
/// 6749 scope-token character set return an error.
pub fn narrow_scope(requested: &str, strict: bool) -> Result<(String, Option<String>)> {
	let mut granted = String::new();
	let mut device_id: Option<&str> = None;

	for token in requested.split_whitespace() {
		let keep = if let Some(id) = DEVICE_SCOPE_PREFIXES
			.iter()
			.find_map(|prefix| token.strip_prefix(prefix))
		{
			if device_id.is_some() {
				return Err!(Request(InvalidParam("more than one device scope requested")));
			}
			if id.is_empty() || !id.bytes().all(is_scope_char) {
				return Err!(Request(InvalidParam("device id contains an invalid character")));
			}

			device_id = Some(id);
			true
		} else {
			token == "openid"
				|| API_SCOPE_PREFIXES
					.iter()
					.any(|prefix| token.starts_with(prefix))
		};

		if keep {
			if !granted.is_empty() {
				granted.push(' ');
			}

			granted.push_str(token);
		} else if strict {
			return Err!(Request(InvalidParam("unsupported scope requested")));
		}
	}

	Ok((granted, device_id.map(ToOwned::to_owned)))
}

/// RFC 6749 appendix A NQCHAR: printable ASCII except space, double quote
/// and backslash. MSC4108 clients use unpadded base64 device ids.
#[inline]
fn is_scope_char(b: u8) -> bool { b.is_ascii_graphic() && !matches!(b, b'"' | b'\\') }

#[cfg(test)]
mod tests {
	use super::narrow_scope;

	#[test]
	fn narrow_scope_keeps_known_drops_unknown() {
		let requested =
			"openid urn:matrix:client:api:* urn:matrix:client:device:ABCDEFGHIJ custom:x";

		let (granted, device) = narrow_scope(requested, false).expect("narrows");

		assert_eq!(granted, "openid urn:matrix:client:api:* urn:matrix:client:device:ABCDEFGHIJ");
		assert_eq!(device.as_deref(), Some("ABCDEFGHIJ"));
	}

	#[test]
	fn narrow_scope_strict_rejects_unknown() {
		narrow_scope("openid custom:x", true).unwrap_err();
		narrow_scope("openid custom:x", false).unwrap();
	}

	#[test]
	fn narrow_scope_accepts_unstable_device_spelling() {
		let scope = "urn:matrix:org.matrix.msc2967.client:device:DEV0123456";
		let (_granted, device) = narrow_scope(scope, false).expect("narrows");

		assert_eq!(device.as_deref(), Some("DEV0123456"));
	}

	#[test]
	fn narrow_scope_rejects_two_device_scopes() {
		let two = "urn:matrix:client:device:AAAAAAAAAA urn:matrix:client:device:BBBBBBBBBB";

		narrow_scope(two, false).unwrap_err();
	}

	#[test]
	fn narrow_scope_accepts_base64_device_id() {
		let scope = "urn:matrix:client:device:wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/V+afGmU5+0";
		let (_granted, device) = narrow_scope(scope, false).expect("narrows");

		assert_eq!(device.as_deref(), Some("wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/V+afGmU5+0"));
	}

	#[test]
	fn narrow_scope_rejects_invalid_device_id() {
		narrow_scope("urn:matrix:client:device:bad\"id", false).unwrap_err();
		narrow_scope("urn:matrix:client:device:bad\\id", false).unwrap_err();
	}
}
