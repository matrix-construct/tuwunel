use std::borrow::Borrow;

use ruma::{
	CanonicalJsonObject, ServerName, ServerSigningKeyId, api::federation::discovery::VerifyKey,
	room_version_rules::RoomVersionRules,
};
use tuwunel_core::{Err, Result, implement};

use super::{PubKeyMap, PubKeys, extract_key};

#[implement(super::Service)]
pub async fn get_event_keys(
	&self,
	object: &CanonicalJsonObject,
	version: &RoomVersionRules,
) -> Result<PubKeyMap> {
	use ruma::signatures::required_keys;

	let required = match required_keys(object, &version.signatures) {
		| Ok(required) => required,
		| Err(e) => {
			return Err!(BadServerResponse("Failed to determine keys required to verify: {e}"));
		},
	};

	let batch = required
		.iter()
		.map(|(s, ids)| (s.borrow(), ids.iter().map(Borrow::borrow)));

	Ok(self.get_pubkeys(batch).await)
}

#[implement(super::Service)]
pub async fn get_pubkeys<'a, S, K>(&self, batch: S) -> PubKeyMap
where
	S: Iterator<Item = (&'a ServerName, K)> + Send,
	K: Iterator<Item = &'a ServerSigningKeyId> + Send,
{
	let mut keys = PubKeyMap::new();
	for (server, key_ids) in batch {
		let pubkeys = self.get_pubkeys_for(server, key_ids).await;
		keys.insert(server.as_str().into(), pubkeys);
	}

	keys
}

#[implement(super::Service)]
pub async fn get_pubkeys_for<'a, I>(&self, origin: &ServerName, key_ids: I) -> PubKeys
where
	I: Iterator<Item = &'a ServerSigningKeyId> + Send,
{
	let mut keys = PubKeys::new();
	for key_id in key_ids {
		if let Ok(verify_key) = self.get_verify_key(origin, key_id).await {
			keys.insert(key_id.as_str().into(), verify_key.key);
		}
	}

	keys
}

#[implement(super::Service)]
pub async fn get_verify_key(
	&self,
	origin: &ServerName,
	key_id: &ServerSigningKeyId,
) -> Result<VerifyKey> {
	let notary_first = self
		.services
		.server
		.config
		.query_trusted_key_servers_first;

	let notary_only = self
		.services
		.server
		.config
		.only_query_trusted_key_servers;

	if let Some(result) = self.verify_keys_for(origin).await.remove(key_id) {
		return Ok(result);
	}

	if notary_first
		&& let Ok(result) = self
			.get_verify_key_from_notaries(origin, key_id)
			.await
	{
		return Ok(result);
	}

	if !notary_only
		&& let Ok(result) = self
			.get_verify_key_from_origin(origin, key_id)
			.await
	{
		return Ok(result);
	}

	if !notary_first
		&& let Ok(result) = self
			.get_verify_key_from_notaries(origin, key_id)
			.await
	{
		return Ok(result);
	}

	Err!(BadServerResponse(debug_error!(
		?key_id,
		?origin,
		"Failed to fetch federation signing-key"
	)))
}

#[implement(super::Service)]
async fn get_verify_key_from_notaries(
	&self,
	origin: &ServerName,
	key_id: &ServerSigningKeyId,
) -> Result<VerifyKey> {
	for notary in &self.services.config.trusted_servers {
		if let Ok(server_keys) = self.notary_request(notary, origin).await {
			for server_key in server_keys.clone() {
				self.add_signing_keys(server_key).await;
			}

			for server_key in server_keys {
				if let Some(result) = extract_key(server_key, key_id) {
					return Ok(result);
				}
			}
		}
	}

	Err!(Request(NotFound("Failed to fetch signing-key from notaries")))
}

#[implement(super::Service)]
async fn get_verify_key_from_origin(
	&self,
	origin: &ServerName,
	key_id: &ServerSigningKeyId,
) -> Result<VerifyKey> {
	if let Ok(server_key) = self.server_request(origin).await {
		self.add_signing_keys(server_key.clone()).await;
		if let Some(result) = extract_key(server_key, key_id) {
			return Ok(result);
		}
	}

	Err!(Request(NotFound("Failed to fetch signing-key from origin")))
}
