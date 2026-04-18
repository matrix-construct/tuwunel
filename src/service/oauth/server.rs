mod auth;
mod client;
mod jwk;
mod signing_key;
mod token;

use std::sync::Arc;

use serde_json::Value as JsonValue;
use tuwunel_core::{Result, debug_info, err, implement, warn};
use tuwunel_database::Map;

pub use self::{
	auth::{AUTH_REQUEST_LIFETIME, AuthCodeSession, AuthRequest},
	client::{ClientRegistration, DcrRequest},
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
}

struct Data {
	oidc_signingkey: Arc<Map>,
	oidcclientid_registration: Arc<Map>,
	oidccode_authsession: Arc<Map>,
	oidcreqid_authrequest: Arc<Map>,
}

impl Server {
	pub(super) fn build(args: &crate::Args<'_>) -> Result<Option<Self>> {
		if !args.server.config.identity_provider.is_empty()
			&& args.server.config.well_known.client.is_none()
		{
			warn!(
				"OIDC server (Next-gen auth) requires `well_known.client` to be configured to \
				 serve your `identity_provider`."
			);
			return Ok(None);
		}

		debug_info!("Initializing OIDC server for next-gen auth (MSC2965)");

		let db = Data {
			oidc_signingkey: args.db["oidc_signingkey"].clone(),
			oidcclientid_registration: args.db["oidcclientid_registration"].clone(),
			oidccode_authsession: args.db["oidccode_authsession"].clone(),
			oidcreqid_authrequest: args.db["oidcreqid_authrequest"].clone(),
		};

		let key = init_signing_key(&db)?;

		Ok(Some(Self {
			services: args.services.clone(),
			db,
			jwk: init_jwk(&key.key_der, &key.key_id)?,
			key,
		}))
	}
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
			if s.ends_with('/') { s } else { s + "/" }
		})
		.ok_or_else(|| {
			err!(Config("well_known.client", "well_known.client must be set for OIDC server"))
		})
}

#[inline]
pub fn extract_device_id(scope: &str) -> Option<String> {
	scope
		.split_whitespace()
		.find_map(|s| s.strip_prefix("urn:matrix:org.matrix.msc2967.client:device:"))
		.map(ToOwned::to_owned)
}
