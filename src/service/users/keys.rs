use std::{collections::BTreeMap, mem};

use futures::{Stream, StreamExt, TryFutureExt};
use ruma::{
	DeviceId, KeyId, OneTimeKeyAlgorithm, OneTimeKeyId, OneTimeKeyName, OwnedKeyId, RoomId, UInt,
	UserId,
	api::client::error::ErrorKind,
	encryption::{CrossSigningKey, DeviceKeys, OneTimeKey},
	serde::Raw,
};
use tuwunel_core::{
	Err, Error, Result, debug_error, err, implement,
	utils::{ReadyExt, stream::TryIgnore, string::Unquoted},
};
use tuwunel_database::{Deserialized, Ignore, Json};

#[implement(super::Service)]
pub async fn add_one_time_keys<'a, Keys>(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	keys: Keys,
) -> Result
where
	Keys: Iterator<Item = (&'a OneTimeKeyId, &'a Raw<OneTimeKey>)> + Send + 'a,
{
	for (id, key) in keys {
		self.add_one_time_key(user_id, device_id, id, key)
			.await
			.ok();
	}

	Ok(())
}

#[implement(super::Service)]
pub async fn add_one_time_key(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	one_time_key_key: &KeyId<OneTimeKeyAlgorithm, OneTimeKeyName>,
	one_time_key_value: &Raw<OneTimeKey>,
) -> Result {
	if !self.device_exists(user_id, device_id).await {
		return Err!(Database(error!(
			?user_id,
			?device_id,
			"User does not exist or device has no metadata."
		)));
	}

	if let Err(e) = one_time_key_value
		.deserialize()
		.map_err(Into::into)
	{
		debug_error!(
			?one_time_key_key,
			?one_time_key_value,
			"Invalid one time key JSON submitted by client, skipping: {e}"
		);

		return Err(e);
	}

	let mut key = user_id.as_bytes().to_vec();
	key.push(0xFF);
	key.extend_from_slice(device_id.as_bytes());
	key.push(0xFF);
	// TODO: Use DeviceKeyId::to_string when it's available (and update everything,
	// because there are no wrapping quotation marks anymore)
	key.extend_from_slice(serde_json::to_string(one_time_key_key)?.as_bytes());

	let count = self.services.globals.next_count();

	self.db
		.onetimekeyid_onetimekeys
		.raw_put(key, Json(one_time_key_value));

	self.db
		.userid_lastonetimekeyupdate
		.raw_put(user_id, *count);

	Ok(())
}

#[implement(super::Service)]
pub async fn last_one_time_keys_update(&self, user_id: &UserId) -> u64 {
	self.db
		.userid_lastonetimekeyupdate
		.get(user_id)
		.await
		.deserialized()
		.unwrap_or(0)
}

#[implement(super::Service)]
pub async fn take_one_time_key(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	key_algorithm: &OneTimeKeyAlgorithm,
) -> Result<(OwnedKeyId<OneTimeKeyAlgorithm, OneTimeKeyName>, Raw<OneTimeKey>)> {
	let count = self.services.globals.next_count();
	self.db
		.userid_lastonetimekeyupdate
		.insert(user_id, count.to_be_bytes());

	let mut prefix = user_id.as_bytes().to_vec();
	prefix.push(0xFF);
	prefix.extend_from_slice(device_id.as_bytes());
	prefix.push(0xFF);
	prefix.push(b'"'); // Annoying quotation mark
	prefix.extend_from_slice(key_algorithm.as_ref().as_bytes());
	prefix.push(b':');

	let one_time_key = self
		.db
		.onetimekeyid_onetimekeys
		.raw_stream_prefix(&prefix)
		.ignore_err()
		.map(|(key, val)| {
			self.db.onetimekeyid_onetimekeys.remove(key);

			let key = key
				.rsplit(|&b| b == 0xFF)
				.next()
				.ok_or_else(|| err!(Database("OneTimeKeyId in db is invalid.")))
				.unwrap();

			let key = serde_json::from_slice(key)
				.map_err(|e| err!(Database("OneTimeKeyId in db is invalid. {e}")))
				.unwrap();

			let val = serde_json::from_slice(val)
				.map_err(|e| err!(Database("OneTimeKeys in db are invalid. {e}")))
				.unwrap();

			(key, val)
		})
		.next()
		.await;

	one_time_key.ok_or_else(|| err!(Request(NotFound("No one-time-key found"))))
}

#[implement(super::Service)]
pub async fn count_one_time_keys(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
) -> BTreeMap<OneTimeKeyAlgorithm, UInt> {
	type KeyVal<'a> = ((Ignore, Ignore, &'a Unquoted), Ignore);

	let mut algorithm_counts = BTreeMap::<OneTimeKeyAlgorithm, _>::new();
	let query = (user_id, device_id);
	self.db
		.onetimekeyid_onetimekeys
		.stream_prefix(&query)
		.ignore_err()
		.ready_for_each(|((Ignore, Ignore, device_key_id), Ignore): KeyVal<'_>| {
			let one_time_key_id: &OneTimeKeyId = device_key_id
				.as_str()
				.try_into()
				.expect("Invalid DeviceKeyID in database");

			let count: &mut UInt = algorithm_counts
				.entry(one_time_key_id.algorithm())
				.or_default();

			*count = count.saturating_add(1_u32.into());
		})
		.await;

	let total = algorithm_counts
		.values()
		.copied()
		.map(TryInto::try_into)
		.filter_map(Result::ok)
		.fold(0_usize, usize::saturating_add);

	if total > self.services.config.one_time_key_limit {
		self.prune_one_time_keys(user_id, device_id).await;
	}

	algorithm_counts
}

#[implement(super::Service)]
pub async fn prune_one_time_keys(&self, user_id: &UserId, device_id: &DeviceId) {
	use tuwunel_database::keyval::Key;

	let query = (user_id, device_id);
	self.db
		.onetimekeyid_onetimekeys
		.keys_prefix(&query)
		.ignore_err()
		.skip(self.services.config.one_time_key_limit)
		.ready_for_each(|key: Key<'_>| {
			self.db.onetimekeyid_onetimekeys.remove(key);
		})
		.await;
}

#[implement(super::Service)]
pub async fn add_device_keys(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	device_keys: &Raw<DeviceKeys>,
) {
	let key = (user_id, device_id);

	self.db.keyid_key.put(key, Json(device_keys));
	self.mark_device_key_update(user_id).await;
}

#[implement(super::Service)]
pub async fn add_cross_signing_keys(
	&self,
	user_id: &UserId,
	master_key: &Option<Raw<CrossSigningKey>>,
	self_signing_key: &Option<Raw<CrossSigningKey>>,
	user_signing_key: &Option<Raw<CrossSigningKey>>,
	notify: bool,
) -> Result {
	// TODO: Check signatures
	let mut prefix = user_id.as_bytes().to_vec();
	prefix.push(0xFF);

	if let Some(master_key) = master_key {
		let (master_key_key, _) = parse_master_key(user_id, master_key)?;

		self.db
			.keyid_key
			.insert(&master_key_key, master_key.json().get().as_bytes());

		self.db
			.userid_masterkeyid
			.insert(user_id.as_bytes(), &master_key_key);
	}

	// Self-signing key
	if let Some(self_signing_key) = self_signing_key {
		let mut self_signing_key_ids = self_signing_key
			.deserialize()
			.map_err(|e| err!(Request(InvalidParam("Invalid self signing key: {e:?}"))))?
			.keys
			.into_values();

		let self_signing_key_id = self_signing_key_ids
			.next()
			.ok_or(Error::BadRequest(
				ErrorKind::InvalidParam,
				"Self signing key contained no key.",
			))?;

		if self_signing_key_ids.next().is_some() {
			return Err(Error::BadRequest(
				ErrorKind::InvalidParam,
				"Self signing key contained more than one key.",
			));
		}

		let mut self_signing_key_key = prefix.clone();
		self_signing_key_key.extend_from_slice(self_signing_key_id.as_bytes());

		self.db
			.keyid_key
			.insert(&self_signing_key_key, self_signing_key.json().get().as_bytes());

		self.db
			.userid_selfsigningkeyid
			.insert(user_id.as_bytes(), &self_signing_key_key);
	}

	// User-signing key
	if let Some(user_signing_key) = user_signing_key {
		let user_signing_key_id = parse_user_signing_key(user_signing_key)?;

		let user_signing_key_key = (user_id, &user_signing_key_id);
		self.db
			.keyid_key
			.put_raw(user_signing_key_key, user_signing_key.json().get().as_bytes());

		self.db
			.userid_usersigningkeyid
			.raw_put(user_id, user_signing_key_key);
	}

	if notify {
		self.mark_device_key_update(user_id).await;
	}

	Ok(())
}

#[implement(super::Service)]
pub async fn sign_key(
	&self,
	target_id: &UserId,
	key_id: &str,
	signature: (String, String),
	sender_id: &UserId,
) -> Result {
	let key = (target_id, key_id);

	let mut cross_signing_key: serde_json::Value = self
		.db
		.keyid_key
		.qry(&key)
		.await
		.map_err(|_| err!(Request(InvalidParam("Tried to sign nonexistent key"))))?
		.deserialized()
		.map_err(|e| err!(Database(debug_warn!("key in keyid_key is invalid: {e:?}"))))?;

	let signatures = cross_signing_key
		.get_mut("signatures")
		.ok_or_else(|| err!(Database(debug_warn!("key in keyid_key has no signatures field"))))?
		.as_object_mut()
		.ok_or_else(|| {
			err!(Database(debug_warn!("key in keyid_key has invalid signatures field.")))
		})?
		.entry(sender_id.to_string())
		.or_insert_with(|| serde_json::Map::new().into());

	signatures
		.as_object_mut()
		.ok_or_else(|| {
			err!(Database(debug_warn!("signatures in keyid_key for a user is invalid.")))
		})?
		.insert(signature.0, signature.1.into());

	let key = (target_id, key_id);
	self.db
		.keyid_key
		.put(key, Json(cross_signing_key));

	self.mark_device_key_update(target_id).await;

	Ok(())
}

#[implement(super::Service)]
#[inline]
pub fn keys_changed<'a>(
	&'a self,
	user_id: &'a UserId,
	from: u64,
	to: Option<u64>,
) -> impl Stream<Item = &UserId> + Send + 'a {
	self.keys_changed_user_or_room(user_id.as_str(), from, to)
		.map(|(user_id, ..)| user_id)
}

#[implement(super::Service)]
#[inline]
pub fn room_keys_changed<'a>(
	&'a self,
	room_id: &'a RoomId,
	from: u64,
	to: Option<u64>,
) -> impl Stream<Item = (&UserId, u64)> + Send + 'a {
	self.keys_changed_user_or_room(room_id.as_str(), from, to)
}

#[implement(super::Service)]
fn keys_changed_user_or_room<'a>(
	&'a self,
	user_or_room_id: &'a str,
	from: u64,
	to: Option<u64>,
) -> impl Stream<Item = (&UserId, u64)> + Send + 'a {
	type KeyVal<'a> = ((&'a str, u64), &'a UserId);

	let to = to.unwrap_or(u64::MAX);
	let start = (user_or_room_id, from.saturating_add(1));
	self.db
		.keychangeid_userid
		.stream_from(&start)
		.ignore_err()
		.ready_take_while(move |((prefix, count), _): &KeyVal<'_>| {
			*prefix == user_or_room_id && *count <= to
		})
		.map(|((_, count), user_id): KeyVal<'_>| (user_id, count))
}

#[implement(super::Service)]
pub async fn mark_device_key_update(&self, user_id: &UserId) {
	let count = self.services.globals.next_count();

	self.services
		.state_cache
		.rooms_joined(user_id)
		// Don't send key updates to unencrypted rooms
		.filter(|room_id| self.services.state_accessor.is_encrypted_room(room_id))
		.ready_for_each(|room_id| {
			let key = (room_id, *count);
			self.db.keychangeid_userid.put_raw(key, user_id);
		})
		.await;

	let key = (user_id, *count);
	self.db.keychangeid_userid.put_raw(key, user_id);
}

#[implement(super::Service)]
pub async fn get_device_keys<'a>(
	&'a self,
	user_id: &'a UserId,
	device_id: &DeviceId,
) -> Result<Raw<DeviceKeys>> {
	let key_id = (user_id, device_id);
	self.db
		.keyid_key
		.qry(&key_id)
		.await
		.deserialized()
}

#[implement(super::Service)]
pub async fn get_key<F>(
	&self,
	key_id: &[u8],
	sender_user: Option<&UserId>,
	user_id: &UserId,
	allowed_signatures: &F,
) -> Result<Raw<CrossSigningKey>>
where
	F: Fn(&UserId) -> bool + Send + Sync,
{
	let key: serde_json::Value = self
		.db
		.keyid_key
		.get(key_id)
		.await
		.deserialized()?;

	let cleaned = clean_signatures(key, sender_user, user_id, allowed_signatures)?;
	let raw_value = serde_json::value::to_raw_value(&cleaned)?;
	Ok(Raw::from_json(raw_value))
}

#[implement(super::Service)]
pub async fn get_master_key<F>(
	&self,
	sender_user: Option<&UserId>,
	user_id: &UserId,
	allowed_signatures: &F,
) -> Result<Raw<CrossSigningKey>>
where
	F: Fn(&UserId) -> bool + Send + Sync,
{
	let key_id = self.db.userid_masterkeyid.get(user_id).await?;

	self.get_key(&key_id, sender_user, user_id, allowed_signatures)
		.await
}

#[implement(super::Service)]
pub async fn get_self_signing_key<F>(
	&self,
	sender_user: Option<&UserId>,
	user_id: &UserId,
	allowed_signatures: &F,
) -> Result<Raw<CrossSigningKey>>
where
	F: Fn(&UserId) -> bool + Send + Sync,
{
	let key_id = self
		.db
		.userid_selfsigningkeyid
		.get(user_id)
		.await?;

	self.get_key(&key_id, sender_user, user_id, allowed_signatures)
		.await
}

#[implement(super::Service)]
pub async fn get_user_signing_key(&self, user_id: &UserId) -> Result<Raw<CrossSigningKey>> {
	self.db
		.userid_usersigningkeyid
		.get(user_id)
		.and_then(|key_id| self.db.keyid_key.get(&*key_id))
		.await
		.deserialized()
}

pub fn parse_master_key(
	user_id: &UserId,
	master_key: &Raw<CrossSigningKey>,
) -> Result<(Vec<u8>, CrossSigningKey)> {
	let mut prefix = user_id.as_bytes().to_vec();
	prefix.push(0xFF);

	let master_key = master_key
		.deserialize()
		.map_err(|_| err!(Request(InvalidParam("Invalid master key"))))?;

	let mut master_key_ids = master_key.keys.values();
	let master_key_id = master_key_ids
		.next()
		.ok_or(err!(Request(InvalidParam("Master key contained no key."))))?;

	if master_key_ids.next().is_some() {
		return Err!(Request(InvalidParam("Master key contained more than one key.")));
	}

	let mut master_key_key = prefix.clone();
	master_key_key.extend_from_slice(master_key_id.as_bytes());

	Ok((master_key_key, master_key))
}

pub(super) fn parse_user_signing_key(user_signing_key: &Raw<CrossSigningKey>) -> Result<String> {
	let mut user_signing_key_ids = user_signing_key
		.deserialize()
		.map_err(|_| err!(Request(InvalidParam("Invalid user signing key"))))?
		.keys
		.into_values();

	let user_signing_key_id = user_signing_key_ids
		.next()
		.ok_or(err!(Request(InvalidParam("User signing key contained no key."))))?;

	if user_signing_key_ids.next().is_some() {
		return Err!(Request(InvalidParam("User signing key contained more than one key.")));
	}

	Ok(user_signing_key_id)
}

/// Ensure that a user only sees signatures from themselves and the target user
fn clean_signatures<F>(
	mut cross_signing_key: serde_json::Value,
	sender_user: Option<&UserId>,
	user_id: &UserId,
	allowed_signatures: &F,
) -> Result<serde_json::Value>
where
	F: Fn(&UserId) -> bool + Send + Sync,
{
	if let Some(signatures) = cross_signing_key
		.get_mut("signatures")
		.and_then(|v| v.as_object_mut())
	{
		// Don't allocate for the full size of the current signatures, but require
		// at most one resize if nothing is dropped
		let new_capacity = signatures.len() / 2;
		for (user, signature) in
			mem::replace(signatures, serde_json::Map::with_capacity(new_capacity))
		{
			let sid = <&UserId>::try_from(user.as_str())
				.map_err(|_| Error::bad_database("Invalid user ID in database."))?;

			if sender_user == Some(user_id) || sid == user_id || allowed_signatures(sid) {
				signatures.insert(user, signature);
			}
		}
	}

	Ok(cross_signing_key)
}
