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

pub type VerifyKeys = BTreeMap<OwnedServerSigningKeyId, VerifyKey>;
pub type PubKeyMap = PublicKeyMap;
pub type PubKeys = PublicKeySet;

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let minimum_valid = Duration::from_secs(3600);

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

#[implement(Service)]
#[inline]
#[must_use]
pub fn keypair(&self) -> &Ed25519KeyPair { &self.keypair }

#[implement(Service)]
#[inline]
#[must_use]
pub fn active_key_id(&self) -> &ServerSigningKeyId { self.active_verify_key().0 }

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

#[implement(Service)]
pub async fn verify_keys_for(&self, origin: &ServerName) -> VerifyKeys {
	let mut keys = self
		.signing_keys_for(origin)
		.await
		.map(|keys| merge_old_keys(keys).verify_keys)
		.unwrap_or(BTreeMap::new());

	if self.services.globals.server_is_ours(origin) {
		keys.extend(self.verify_keys.clone().into_iter());
	}

	keys
}

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
