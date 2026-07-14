use std::{collections::BTreeMap, mem};

use futures::{Stream, StreamExt, TryFutureExt, pin_mut};
use ruma::{
	DeviceId, KeyId, OneTimeKeyAlgorithm, OneTimeKeyId, OneTimeKeyName, OwnedKeyId,
	OwnedOneTimeKeyId, OwnedRoomId, OwnedServerName, RoomId, UInt, UserId,
	encryption::{CrossSigningKey, DeviceKeys, OneTimeKey},
	serde::Raw,
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Result, debug_error, err, implement,
	smallvec::SmallVec,
	utils::{
		BoolExt, IterStream, ReadyExt,
		result::LogErr,
		stream::{BroadbandExt, TryIgnore},
	},
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Json};

type Servers = SmallVec<[OwnedServerName; 1]>;

/// MSC2732: row stored under `(user, device, algorithm)` in
/// `userdeviceidalgorithm_fallback`. Fallback keys are not deleted on
/// claim; the row is rewritten with `used = true`.
#[derive(Debug, Deserialize, Serialize)]
struct FallbackEntry {
	key_id: OwnedOneTimeKeyId,
	key: Raw<OneTimeKey>,
	used: bool,
}

/// Row-key shape of `onetimekeyid4225_otk`: per-device pool keyed by
/// upload-order count for MSC4225 ordering.
type OtkRowKey<'a> = (&'a UserId, &'a DeviceId, u64, &'a OneTimeKeyId);

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
	let Some(otk) = self.db.onetimekeyid4225_otk.as_ref() else {
		return Err!(Database("one-time-key column unavailable"));
	};

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

	// Racy dedup: two concurrent uploads of the same id can both pass this
	// check and produce duplicate rows that persist until aged out by prune.
	let prefix = (user_id, device_id, Interfix);
	let already_present = otk
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_any(|(.., id): OtkRowKey<'_>| id == one_time_key_key)
		.await;

	if already_present {
		return Ok(());
	}

	let count = self.services.globals.next_count();

	// MSC4225: RocksDB iterates the (user, device) prefix in count_be ascending
	// order, so /keys/claim issues one-time keys in the order they were uploaded.
	otk.put(
		(user_id, device_id, *count, one_time_key_key.as_str()),
		Json(one_time_key_value),
	);

	self.db
		.userid_lastonetimekeyupdate
		.raw_put(user_id, *count);

	Ok(())
}

#[implement(super::Service)]
pub async fn add_fallback_keys<'a, Keys>(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	keys: Keys,
) -> Result
where
	Keys: Iterator<Item = (&'a OneTimeKeyId, &'a Raw<OneTimeKey>)> + Send + 'a,
{
	for (id, key) in keys {
		self.add_fallback_key(user_id, device_id, id, key)
			.await
			.ok();
	}

	Ok(())
}

#[implement(super::Service)]
pub async fn add_fallback_key(
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
			"Invalid fallback key JSON submitted by client, skipping: {e}"
		);

		return Err(e);
	}

	let entry = FallbackEntry {
		key_id: one_time_key_key.to_owned(),
		key: one_time_key_value.clone(),
		used: false,
	};

	let key = (user_id, device_id, one_time_key_key.algorithm());
	self.db
		.userdeviceidalgorithm_fallback
		.put(key, Json(&entry));

	let count = self.services.globals.next_count();
	self.db
		.userid_lastonetimekeyupdate
		.raw_put(user_id, *count);

	Ok(())
}

#[implement(super::Service)]
pub async fn take_fallback_key(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	algorithm: &OneTimeKeyAlgorithm,
) -> Result<(OwnedKeyId<OneTimeKeyAlgorithm, OneTimeKeyName>, Raw<OneTimeKey>)> {
	let key = (user_id, device_id, algorithm);
	let entry: FallbackEntry = self
		.db
		.userdeviceidalgorithm_fallback
		.qry(&key)
		.await
		.deserialized::<Json<_>>()
		.map(|Json(entry)| entry)
		.map_err(|_| err!(Request(NotFound("No fallback key found"))))?;

	let updated = FallbackEntry { used: true, ..entry };
	self.db
		.userdeviceidalgorithm_fallback
		.put(key, Json(&updated));

	Ok((updated.key_id, updated.key))
}

#[implement(super::Service)]
pub fn unused_fallback_key_algorithms<'a>(
	&'a self,
	user_id: &'a UserId,
	device_id: &'a DeviceId,
) -> impl Stream<Item = OneTimeKeyAlgorithm> + Send + 'a {
	type KeyVal = ((Ignore, Ignore, OneTimeKeyAlgorithm), Json<FallbackEntry>);

	let prefix = (user_id, device_id);
	self.db
		.userdeviceidalgorithm_fallback
		.stream_prefix(&prefix)
		.ignore_err()
		.ready_filter_map(|((_, _, algorithm), Json(entry)): KeyVal| {
			entry.used.is_false().then_some(algorithm)
		})
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
	let Some(otk) = self.db.onetimekeyid4225_otk.as_ref() else {
		return Err!(Request(NotFound("No one-time-key found")));
	};

	let update_count = self.services.globals.next_count();
	self.db
		.userid_lastonetimekeyupdate
		.insert(user_id, update_count.to_be_bytes());

	let prefix = (user_id, device_id, Interfix);
	let one_time_keys = otk
		.stream_prefix(&prefix)
		.ignore_err()
		.ready_filter(|(row, _): &(OtkRowKey<'_>, &[u8])| row.3.algorithm() == *key_algorithm);

	pin_mut!(one_time_keys);
	let ((user_id, device_id, count, id), val) = one_time_keys
		.next()
		.await
		.ok_or_else(|| err!(Request(NotFound("No one-time-key found"))))?;

	otk.del((user_id, device_id, count, id));

	Ok((id.into(), serde_json::from_slice(val)?))
}

#[implement(super::Service)]
pub async fn count_one_time_keys(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
) -> BTreeMap<OneTimeKeyAlgorithm, UInt> {
	let Some(otk) = self.db.onetimekeyid4225_otk.as_ref() else {
		// Without the MSC4225 column this node cannot observe the authoritative
		// pool, so preserve "unknown" instead of falsely reporting zero keys.
		return BTreeMap::new();
	};

	let prefix = (user_id, device_id, Interfix);
	let algorithm_counts: BTreeMap<OneTimeKeyAlgorithm, UInt> = otk
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_fold(BTreeMap::new(), |mut acc, (.., id): OtkRowKey<'_>| {
			let count: &mut UInt = acc.entry(id.algorithm()).or_default();
			*count = count.saturating_add(1_u32.into());
			acc
		})
		.await;

	let total = algorithm_counts
		.values()
		.copied()
		.map(TryInto::try_into)
		.filter_map(Result::ok)
		.fold(0_usize, usize::saturating_add);

	let limit = self.services.config.one_time_key_limit;
	if let Some(excess) = total.checked_sub(limit).filter(|&n| n > 0) {
		self.prune_one_time_keys(user_id, device_id, excess)
			.await;
	}

	complete_one_time_key_counts(algorithm_counts)
}

/// Keep zero-count algorithms visible to clients after an OTK pool is drained.
///
/// An empty map is omitted from `/sync` by ruma. Some clients, including nio,
/// interpret an omitted count as "unknown" and therefore do not replenish a
/// previously-uploaded Olm account. `curve25519` is retained for compatibility
/// with clients which still require the legacy count in `/keys/upload`.
fn complete_one_time_key_counts(
	mut counts: BTreeMap<OneTimeKeyAlgorithm, UInt>,
) -> BTreeMap<OneTimeKeyAlgorithm, UInt> {
	counts
		.entry(OneTimeKeyAlgorithm::from("curve25519"))
		.or_default();
	counts
		.entry(OneTimeKeyAlgorithm::SignedCurve25519)
		.or_default();
	counts
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn empty_one_time_key_counts_include_supported_zeroes() {
		let counts = complete_one_time_key_counts(BTreeMap::new());

		assert_eq!(
			counts.get(&OneTimeKeyAlgorithm::from("curve25519")),
			Some(&UInt::from(0_u32))
		);
		assert_eq!(counts.get(&OneTimeKeyAlgorithm::SignedCurve25519), Some(&UInt::from(0_u32)));
	}

	#[test]
	fn existing_one_time_key_counts_are_preserved() {
		let mut counts = BTreeMap::new();
		counts.insert(OneTimeKeyAlgorithm::from("curve25519"), UInt::from(11_u32));
		counts.insert(OneTimeKeyAlgorithm::SignedCurve25519, UInt::from(17_u32));

		let counts = complete_one_time_key_counts(counts);

		assert_eq!(
			counts.get(&OneTimeKeyAlgorithm::from("curve25519")),
			Some(&UInt::from(11_u32))
		);
		assert_eq!(counts.get(&OneTimeKeyAlgorithm::SignedCurve25519), Some(&UInt::from(17_u32)));
	}
}

/// MSC4225: drop the `excess` oldest rows for this `(user, device)`. Forward
/// iteration over the prefix runs in count_be ascending order, so
/// `take(excess)` yields the earliest-uploaded rows.
#[implement(super::Service)]
pub async fn prune_one_time_keys(&self, user_id: &UserId, device_id: &DeviceId, excess: usize) {
	let Some(otk) = self.db.onetimekeyid4225_otk.as_ref() else {
		return;
	};

	let prefix = (user_id, device_id, Interfix);
	otk.keys_prefix(&prefix)
		.ignore_err()
		.take(excess)
		.ready_for_each(|row: OtkRowKey<'_>| {
			otk.del(row);
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
			.ok_or_else(|| err!(Request(InvalidParam("Self signing key contained no key."))))?;

		if self_signing_key_ids.next().is_some() {
			return Err!(Request(InvalidParam("Self signing key contained more than one key.")));
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
	let update_all_rooms = !self
		.services
		.config
		.device_key_update_encrypted_rooms_only;

	let all_or_is_encrypted = async |room_id: &RoomId| {
		update_all_rooms
			|| self
				.services
				.state_accessor
				.is_encrypted_room(room_id)
				.await
	};

	let count = self.services.globals.next_count();
	let user_key = (user_id, *count);

	self.db
		.keychangeid_userid
		.put_raw(user_key, user_id);
	self.services
		.state_cache
		.rooms_joined(user_id)
		.filter(|room_id| all_or_is_encrypted(*room_id))
		.ready_for_each(|room_id| {
			let room_key = (room_id, *count);
			self.db
				.keychangeid_userid
				.put_raw(room_key, user_id);
		})
		.await;

	self.services
		.sending
		.send_device_list_appservices(user_id, *count)
		.await
		.log_err()
		.ok();

	if !self.services.globals.user_is_local(user_id) {
		return;
	}

	// device_list_update EDUs reach remote servers only on a sender flush.
	let mut servers: Servers = self
		.services
		.state_cache
		.rooms_joined(user_id)
		.filter(|room_id| all_or_is_encrypted(*room_id))
		.map(ToOwned::to_owned)
		.broad_then(async |room_id: OwnedRoomId| {
			self.services
				.state_cache
				.room_servers(&room_id)
				.ready_filter(|server| !self.services.globals.server_is_ours(server))
				.map(ToOwned::to_owned)
				.collect()
				.await
		})
		.flat_map(|servers: Vec<OwnedServerName>| servers.into_iter().stream())
		.collect()
		.await;

	servers.sort_unstable();
	servers.dedup();

	self.services
		.sending
		.flush_servers(servers.iter().map(|server| &**server).stream())
		.await
		.expect("device key update flush failed");
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
				.map_err(|e| err!(Database("Invalid user ID in database: {e}")))?;

			if sender_user == Some(user_id) || sid == user_id || allowed_signatures(sid) {
				signatures.insert(user, signature);
			}
		}
	}

	Ok(cross_signing_key)
}
