//! Federation signing-key storage, acquisition, signing, and verification.
//!
//! The service loads the local Ed25519 identity, caches remote current and old
//! verify keys, fetches missing keys from origins or configured notaries, and
//! supplies the cryptographic operations used by federation event handling.

mod acquire;
mod get;
mod keypair;
mod request;
mod sign;
mod verify;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures::StreamExt;
use ruma::{
	CanonicalJsonObject, MilliSecondsSinceUnixEpoch, OwnedServerSigningKeyId, ServerName,
	ServerSigningKeyId,
	api::federation::discovery::{ServerSigningKeys, VerifyKey},
	room_version_rules::RoomVersionRules,
	serde::Raw,
	signatures::{Ed25519KeyPair, PublicKeyMap, PublicKeySet},
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{
	Result, implement,
	utils::{IterStream, timepoint_from_now},
};
use tuwunel_database::{Deserialized, Json, Map};

/// Manages the local signing identity and cached remote verification keys.
///
/// Cached keys are retained by key ID without enforcing `valid_until_ts` on
/// reads. Missing keys can be acquired from remote origins or trusted notaries.
pub struct Service {
	keypair: Box<Ed25519KeyPair>,
	verify_keys: VerifyKeys,
	minimum_valid: Duration,
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

struct Data {
	server_signingkeys: Arc<Map>,
}

/// Verify keys indexed by Matrix server signing-key ID.
///
/// Current and retired keys can be merged into this representation for event
/// verification.
pub type VerifyKeys = BTreeMap<OwnedServerSigningKeyId, VerifyKey>;

/// Public keys grouped first by server name and then by key ID.
///
/// This is the map shape accepted by ruma's signature verification helpers.
pub type PubKeyMap = PublicKeyMap;

/// Public keys for one server, indexed by textual key ID.
///
/// Values contain the decoded public-key material expected by ruma.
pub type PubKeys = PublicKeySet;

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let minimum_valid = Duration::from_hours(1);

		let (keypair, verify_keys) = keypair::init(args.db)?;
		debug_assert!(verify_keys.len() == 1, "only one active verify_key supported");

		Ok(Arc::new(Self {
			keypair,
			verify_keys,
			minimum_valid,
			services: args.services.clone(),
			db: Data {
				server_signingkeys: args.db["server_signingkeys"].clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Returns the local Ed25519 signing keypair.
///
/// The keypair is loaded or generated when the service is built and remains
/// fixed for the service lifetime.
#[implement(Service)]
#[inline]
#[must_use]
pub fn keypair(&self) -> &Ed25519KeyPair { &self.keypair }

/// Returns the signing-key ID for the active local verify key.
///
/// This delegates to [`Self::active_verify_key`] and therefore panics if the
/// service was initialized without an active key.
#[implement(Service)]
#[inline]
#[must_use]
pub fn active_key_id(&self) -> &ServerSigningKeyId { self.active_verify_key().0 }

/// Returns the active local signing-key ID and verify key.
///
/// Initialization normally supplies exactly one entry. A missing entry panics,
/// and debug builds also assert that no second active key exists.
#[implement(Service)]
#[inline]
#[must_use]
pub fn active_verify_key(&self) -> (&ServerSigningKeyId, &VerifyKey) {
	debug_assert!(self.verify_keys.len() <= 1, "more than one active verify_key");
	self.verify_keys
		.iter()
		.next()
		.map(|(id, key)| (id.as_ref(), key))
		.expect("missing active verify_key")
}

/// Merges a fetched signing-key document into the local cache.
///
/// Only current and old verify-key maps are retained from the incoming document;
/// its signatures and validity timestamp are not preserved. The read, merge,
/// and write sequence is not atomic.
#[implement(Service)]
async fn add_signing_keys(&self, new_keys: ServerSigningKeys) {
	let origin = &new_keys.server_name;

	// (timo) Not atomic, but this is not critical
	let mut keys: ServerSigningKeys = self
		.db
		.server_signingkeys
		.get(origin)
		.await
		.deserialized()
		.unwrap_or_else(|_| {
			// Just insert "now", it doesn't matter
			ServerSigningKeys::new(origin.to_owned(), MilliSecondsSinceUnixEpoch::now())
		});

	keys.verify_keys.extend(new_keys.verify_keys);
	keys.old_verify_keys
		.extend(new_keys.old_verify_keys);

	self.db
		.server_signingkeys
		.raw_put(origin, Json(&keys));
}

/// Checks whether every signature key required by an event is cached.
///
/// Invalid signature metadata, database errors, and malformed stored key data
/// all produce `false`; this method never fetches missing keys.
#[implement(Service)]
pub async fn required_keys_exist(
	&self,
	object: &CanonicalJsonObject,
	rules: &RoomVersionRules,
) -> bool {
	use ruma::signatures::required_keys;

	let Ok(required_keys) = required_keys(object, &rules.signatures) else {
		return false;
	};

	required_keys
		.iter()
		.flat_map(|(server, key_ids)| key_ids.iter().map(move |key_id| (server, key_id)))
		.stream()
		.all(|(server, key_id)| self.verify_key_exists(server, key_id))
		.await
}

/// Checks whether one current or retired verify key is cached for a server.
///
/// The check is based on key-ID presence only and does not evaluate the stored
/// key document's validity interval. Read or decoding errors produce `false`.
#[implement(Service)]
pub async fn verify_key_exists(&self, origin: &ServerName, key_id: &ServerSigningKeyId) -> bool {
	type KeysMap<'a> = BTreeMap<&'a ServerSigningKeyId, &'a RawJsonValue>;

	let Ok(keys) = self
		.db
		.server_signingkeys
		.get(origin)
		.await
		.deserialized::<Raw<ServerSigningKeys>>()
	else {
		return false;
	};

	if let Ok(Some(verify_keys)) = keys.get_field::<KeysMap<'_>>("verify_keys")
		&& verify_keys.contains_key(key_id)
	{
		return true;
	}

	if let Ok(Some(old_verify_keys)) = keys.get_field::<KeysMap<'_>>("old_verify_keys")
		&& old_verify_keys.contains_key(key_id)
	{
		return true;
	}

	false
}

/// Returns all cached verify keys usable for a server.
///
/// Retired keys are converted and merged with current keys. Storage errors are
/// suppressed to an empty map, and the local active key is added for our names.
#[implement(Service)]
pub async fn verify_keys_for(&self, origin: &ServerName) -> VerifyKeys {
	let mut keys = self
		.signing_keys_for(origin)
		.await
		.map(|keys| merge_old_keys(keys).verify_keys)
		.unwrap_or(BTreeMap::new());

	if self.services.globals.server_is_ours(origin) {
		keys.extend(self.verify_keys.clone());
	}

	keys
}

/// Loads the cached signing-key document for a server.
///
/// The returned document reflects the service's merged cache representation;
/// reads do not enforce `valid_until_ts`. Acquisition currently preserves key
/// maps but not incoming document signatures or validity metadata.
#[implement(Service)]
pub async fn signing_keys_for(&self, origin: &ServerName) -> Result<ServerSigningKeys> {
	self.db
		.server_signingkeys
		.get(origin)
		.await
		.deserialized()
}

#[implement(Service)]
fn minimum_valid_ts(&self) -> MilliSecondsSinceUnixEpoch {
	let timepoint =
		timepoint_from_now(self.minimum_valid).expect("SystemTime should not overflow");

	MilliSecondsSinceUnixEpoch::from_system_time(timepoint).expect("UInt should not overflow")
}

fn merge_old_keys(mut keys: ServerSigningKeys) -> ServerSigningKeys {
	keys.verify_keys.extend(
		keys.old_verify_keys
			.clone()
			.into_iter()
			.map(|(key_id, old)| (key_id, VerifyKey::new(old.key))),
	);

	keys
}

fn extract_key(mut keys: ServerSigningKeys, key_id: &ServerSigningKeyId) -> Option<VerifyKey> {
	keys.verify_keys.remove(key_id).or_else(|| {
		keys.old_verify_keys
			.remove(key_id)
			.map(|old| VerifyKey::new(old.key))
	})
}

fn key_exists(keys: &ServerSigningKeys, key_id: &ServerSigningKeyId) -> bool {
	keys.verify_keys.contains_key(key_id) || keys.old_verify_keys.contains_key(key_id)
}
