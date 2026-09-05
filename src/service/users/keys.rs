use std::{collections::BTreeMap, mem, ops::Deref, sync::Arc};

use futures::{Stream, StreamExt, TryFutureExt, future::join4, pin_mut};
use ruma::{
	AnyKeyName, DeviceId, KeyId, OneTimeKeyAlgorithm, OneTimeKeyId, OneTimeKeyName, OwnedKeyId,
	OwnedOneTimeKeyId, OwnedRoomId, OwnedServerName, RoomId, SigningKeyId, UInt, UserId,
	encryption::{CrossSigningKey, DeviceKeys, OneTimeKey},
	serde::{Base64, Raw, base64::Standard},
	signatures::{
		VerificationError, to_canonical_json_string_for_signing, verify_canonical_json_bytes,
	},
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Error, Result,
	debug::INFO_SPAN_LEVEL,
	debug_error, err, implement,
	smallvec::SmallVec,
	utils::{
		BoolExt, IterStream, ReadyExt,
		result::LogErr,
		stream::{BroadbandExt, TryIgnore},
		to_canonical_object,
	},
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Json, KeyBuf, Map, Txn, serialize_key};

type Servers = SmallVec<[OwnedServerName; 1]>;
type Signatures = SmallVec<[(String, String); 1]>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyRole {
	Device,
	CrossSigningRoot,
	SelfSigning,
	UserSigning,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignatureWrite {
	Merge,
	ReplaceSender,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignatureAction {
	Ignore,
	Reject,
	Write(KeyRole, SignatureWrite),
}

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
pub async fn add_one_time_keys(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	keys: &BTreeMap<OwnedOneTimeKeyId, Raw<OneTimeKey>>,
	limit: usize,
) -> Result {
	let mut txn = self.services.db.txn();
	// Hold the oldest permit so the retirement frontier cannot pass this batch
	// before commit.
	let mut oldest_count = None;
	let mut last_count = None;

	for (id, key) in keys.iter().take(limit) {
		let Ok(Some(count)) = self
			.add_one_time_key(user_id, device_id, id, key, &mut txn)
			.await
		else {
			continue;
		};

		last_count = Some(*count);
		oldest_count = oldest_count.or(Some(count));
	}

	if let Some(count) = last_count {
		txn.raw_put(&self.db.userid_lastonetimekeyupdate, user_id, count);
	}

	txn.execute();
	drop(oldest_count);

	Ok(())
}

#[implement(super::Service)]
pub async fn add_one_time_key(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	one_time_key_key: &KeyId<OneTimeKeyAlgorithm, OneTimeKeyName>,
	one_time_key_value: &Raw<OneTimeKey>,
	txn: &mut Txn,
) -> Result<Option<impl Deref<Target = u64> + Send + use<>>> {
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
		return Ok(None);
	}

	let count = self.services.globals.next_count();

	// MSC4225: RocksDB iterates the (user, device) prefix in count_be ascending
	// order, so /keys/claim issues one-time keys in the order they were uploaded.
	txn.put(
		otk,
		(user_id, device_id, *count, one_time_key_key.as_str()),
		Json(one_time_key_value),
	);

	Ok(Some(count))
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
	let mut txn = self.services.db.txn();
	// Hold the oldest permit so the retirement frontier cannot pass this batch
	// before commit.
	let mut oldest_count = None;
	let mut last_count = None;

	for (id, key) in keys {
		let Ok(count) = self
			.add_fallback_key(user_id, device_id, id, key, &mut txn)
			.await
		else {
			continue;
		};

		last_count = Some(*count);
		oldest_count = oldest_count.or(Some(count));
	}

	if let Some(count) = last_count {
		txn.raw_put(&self.db.userid_lastonetimekeyupdate, user_id, count);
	}

	txn.execute();
	drop(oldest_count);

	Ok(())
}

#[implement(super::Service)]
pub async fn add_fallback_key(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	one_time_key_key: &KeyId<OneTimeKeyAlgorithm, OneTimeKeyName>,
	one_time_key_value: &Raw<OneTimeKey>,
	txn: &mut Txn,
) -> Result<impl Deref<Target = u64> + Send + use<>> {
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
	let count = self.services.globals.next_count();

	txn.put(&self.db.userdeviceidalgorithm_fallback, key, Json(&entry));

	Ok(count)
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
/// An empty map is omitted from `/sync` by ruma. Some clients interpret an
/// omitted count as "unknown" and therefore do not replenish a
/// previously-uploaded Olm account. Only `signed_curve25519` is seeded,
/// matching Synapse; clients do not maintain unsigned curve25519 keys.
fn complete_one_time_key_counts(
	mut counts: BTreeMap<OneTimeKeyAlgorithm, UInt>,
) -> BTreeMap<OneTimeKeyAlgorithm, UInt> {
	counts
		.entry(OneTimeKeyAlgorithm::SignedCurve25519)
		.or_default();
	counts
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
/// Drops every one-time key still outstanding for this `(user, device)`.
///
/// Device removal has to reach the pool as well as the MSC2732 fallback key,
/// or a claim against the deleted device still succeeds from the rows left
/// behind.
#[implement(super::Service)]
pub async fn remove_one_time_keys(&self, user_id: &UserId, device_id: &DeviceId) {
	let Some(otk) = self.db.onetimekeyid4225_otk.as_ref() else {
		return;
	};

	otk.del_prefix(&(user_id, device_id, Interfix))
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
	{
		let master_key_key = master_key
			.as_ref()
			.map(|master_key| parse_master_key(user_id, master_key).map(|(key, _)| key))
			.transpose()?;

		let self_signing_key_key = self_signing_key
			.as_ref()
			.map(|self_signing_key| parse_self_signing_key(user_id, self_signing_key))
			.transpose()?;

		let user_signing_key_id = user_signing_key
			.as_ref()
			.map(parse_user_signing_key)
			.transpose()?;

		let mut txn = self.services.db.txn();

		if let Some((master_key, master_key_key)) =
			master_key.as_ref().zip(master_key_key.as_ref())
		{
			txn.insert_raw(
				&self.db.keyid_key,
				master_key_key,
				master_key.json().get().as_bytes(),
			);
			txn.insert_raw(&self.db.userid_masterkeyid, user_id.as_bytes(), master_key_key);
		}

		if let Some((self_signing_key, self_signing_key_key)) = self_signing_key
			.as_ref()
			.zip(self_signing_key_key.as_ref())
		{
			txn.insert_raw(
				&self.db.keyid_key,
				self_signing_key_key,
				self_signing_key.json().get(),
			);
			txn.insert_raw(
				&self.db.userid_selfsigningkeyid,
				user_id.as_bytes(),
				self_signing_key_key,
			);
		}

		if let Some((user_signing_key, user_signing_key_id)) = user_signing_key
			.as_ref()
			.zip(user_signing_key_id.as_ref())
		{
			let user_signing_key_key = (user_id, user_signing_key_id);

			txn.put_raw(
				&self.db.keyid_key,
				user_signing_key_key,
				user_signing_key.json().get().as_bytes(),
			);

			txn.raw_put(&self.db.userid_usersigningkeyid, user_id, user_signing_key_key);
		}

		txn.execute();
	};

	if notify {
		self.mark_device_key_update(user_id).await;
	}

	Ok(())
}

fn parse_self_signing_key(
	user_id: &UserId,
	self_signing_key: &Raw<CrossSigningKey>,
) -> Result<KeyBuf> {
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

	serialize_key((user_id, self_signing_key_id))
}

#[implement(super::Service)]
pub async fn sign_key(
	&self,
	target_id: &UserId,
	key_id: &str,
	signatures: Signatures,
	sender_id: &UserId,
) -> Result {
	let key = (target_id, key_id);

	let mut target_key: serde_json::Value = self
		.db
		.keyid_key
		.qry(&key)
		.await
		.map_err(|error| match error {
			| error if error.is_not_found() =>
				err!(Request(NotFound("Tried to sign nonexistent key"))),
			| error => error,
		})?
		.deserialized()
		.map_err(|e| err!(Database(debug_warn!("key in keyid_key is invalid: {e:?}"))))?;

	let target_role = self
		.uploaded_key_role(target_id, key_id)
		.await?
		.ok_or_else(|| err!(Request(NotFound("Unknown device"))))?;

	if !key_matches_role(&target_key, target_id, key_id, target_role) {
		return Err!(Request(NotFound("Unknown device")));
	}

	let same_user = sender_id == target_id;
	let mut canonical = None;
	let mut accepted = false;
	let mut changed = false;

	for (signature_id, signature) in signatures {
		let signature_key_id = <&SigningKeyId<AnyKeyName>>::try_from(signature_id.as_str())
			.map_err(|source| VerificationError::ParseIdentifier {
				identifier_type: "signing key ID",
				source,
			})?;

		let signer_role = self
			.uploaded_key_role(sender_id, signature_key_id.key_name().as_str())
			.await?;

		let (signer_role, write) = match signature_action(same_user, target_role, signer_role) {
			| SignatureAction::Ignore => continue,
			| SignatureAction::Reject => return Err!(Request(NotFound("Unknown device"))),
			| SignatureAction::Write(signer_role, write) => (signer_role, write),
		};

		if canonical.is_none() {
			canonical = Some(canonical_key(&target_key)?);
		}

		let canonical = canonical
			.as_deref()
			.ok_or_else(|| err!(Database("canonical key was not initialized")))?;

		self.verify_key_signature(
			sender_id,
			signature_key_id,
			signer_role,
			&signature,
			canonical.as_bytes(),
		)
		.await?;

		changed |= match write {
			| SignatureWrite::Merge =>
				insert_signatures(&mut target_key, sender_id, [(signature_id, signature)])?,
			| SignatureWrite::ReplaceSender =>
				replace_signatures(&mut target_key, sender_id, (signature_id, signature))?,
		};

		accepted = true;
	}

	if !accepted {
		return Err!(Request(NotFound("Unknown device")));
	}

	if !changed {
		return Ok(());
	}

	let key = (target_id, key_id);
	self.db.keyid_key.put(key, Json(target_key));

	self.mark_device_key_update(target_id).await;

	Ok(())
}

#[implement(super::Service)]
async fn uploaded_key_role(&self, user_id: &UserId, key_id: &str) -> Result<Option<KeyRole>> {
	let row_key = serialize_key((user_id, key_id))?;
	let device_id: &DeviceId = key_id.into();

	let (device, root, self_signing, user_signing) = join4(
		self.device_exists(user_id, device_id),
		pointer_matches(&self.db.userid_masterkeyid, user_id, row_key.as_slice()),
		pointer_matches(&self.db.userid_selfsigningkeyid, user_id, row_key.as_slice()),
		pointer_matches(&self.db.userid_usersigningkeyid, user_id, row_key.as_slice()),
	)
	.await;

	Ok(key_role([device, root?, self_signing?, user_signing?]))
}

#[tracing::instrument(
	level = "trace",
	skip_all,
	fields(
		user = %user_id,
	)
)]
async fn pointer_matches(map: &Arc<Map>, user_id: &UserId, row_key: &[u8]) -> Result<bool> {
	match map.get(user_id).await {
		| Ok(pointer) => Ok(&*pointer == row_key),
		| Err(error) if error.is_not_found() => Ok(false),
		| Err(error) => Err(error),
	}
}

fn key_role([device, root, self_signing, user_signing]: [bool; 4]) -> Option<KeyRole> {
	match (device, root, self_signing, user_signing) {
		| (true, false, false, false) => Some(KeyRole::Device),
		| (false, true, false, false) => Some(KeyRole::CrossSigningRoot),
		| (false, false, true, false) => Some(KeyRole::SelfSigning),
		| (false, false, false, true) => Some(KeyRole::UserSigning),
		| _ => None,
	}
}

fn key_matches_role(
	key: &serde_json::Value,
	user_id: &UserId,
	key_id: &str,
	role: KeyRole,
) -> bool {
	if key
		.get("user_id")
		.and_then(serde_json::Value::as_str)
		!= Some(user_id.as_str())
	{
		return false;
	}

	match role {
		| KeyRole::Device =>
			key.get("device_id")
				.and_then(serde_json::Value::as_str)
				== Some(key_id),
		| KeyRole::CrossSigningRoot | KeyRole::SelfSigning | KeyRole::UserSigning =>
			key.get("device_id").is_none()
				&& key
					.get("keys")
					.and_then(serde_json::Value::as_object)
					.is_some_and(|keys| {
						keys.len() == 1
							&& keys
								.values()
								.any(|value| value.as_str() == Some(key_id))
					}),
	}
}

fn signature_action(
	same_user: bool,
	target: KeyRole,
	signer: Option<KeyRole>,
) -> SignatureAction {
	match (same_user, target, signer) {
		| (true, KeyRole::CrossSigningRoot, Some(KeyRole::Device)) =>
			SignatureAction::Write(KeyRole::Device, SignatureWrite::Merge),
		| (true, KeyRole::CrossSigningRoot, _) => SignatureAction::Reject,
		| (true, KeyRole::Device, Some(KeyRole::SelfSigning)) =>
			SignatureAction::Write(KeyRole::SelfSigning, SignatureWrite::Merge),
		| (false, KeyRole::CrossSigningRoot, Some(KeyRole::UserSigning)) =>
			SignatureAction::Write(KeyRole::UserSigning, SignatureWrite::ReplaceSender),
		| _ => SignatureAction::Ignore,
	}
}

fn canonical_key(key: &serde_json::Value) -> Result<String> {
	let key = to_canonical_object(key)?;

	Ok(to_canonical_json_string_for_signing(&key)?)
}

#[implement(super::Service)]
#[tracing::instrument(
	level = "trace",
	skip_all,
	fields(
		sender = %sender_id,
		signing_key_id = %key_id,
		?role,
	)
)]
async fn verify_key_signature(
	&self,
	sender_id: &UserId,
	key_id: &SigningKeyId<AnyKeyName>,
	role: KeyRole,
	signature: &str,
	canonical: &[u8],
) -> Result {
	let signing_key: serde_json::Value = self
		.db
		.keyid_key
		.qry(&(sender_id, key_id.key_name().as_str()))
		.map_err(|error| match error {
			| error if error.is_not_found() =>
				VerificationError::NoPublicKeysForEntity(sender_id.to_string()).into(),
			| error => error,
		})
		.await?
		.deserialized()
		.map_err(|e| err!(Database(debug_warn!("key in keyid_key is invalid: {e:?}"))))?;

	if !key_matches_role(&signing_key, sender_id, key_id.key_name().as_str(), role) {
		return Err(VerificationError::NoPublicKeysForEntity(sender_id.to_string()).into());
	}

	let public_key = signing_key
		.get("keys")
		.and_then(|keys| keys.get(key_id.as_str()))
		.and_then(serde_json::Value::as_str)
		.ok_or_else(|| {
			Error::from(VerificationError::NoPublicKeysForEntity(sender_id.to_string()))
		})?;

	verify_signature(sender_id, key_id, public_key, signature, canonical)
}

fn verify_signature(
	sender_id: &UserId,
	key_id: &SigningKeyId<AnyKeyName>,
	public_key: &str,
	signature: &str,
	canonical: &[u8],
) -> Result {
	let public_key = Base64::<Standard>::parse(public_key)
		.map_err(|_| VerificationError::NoPublicKeysForEntity(sender_id.to_string()))?;

	let signature = Base64::<Standard>::parse(signature).map_err(|source| {
		VerificationError::InvalidBase64Signature {
			path: format!("signatures.{sender_id}.{key_id}"),
			source,
		}
	})?;

	verify_canonical_json_bytes(
		&key_id.algorithm(),
		public_key.as_bytes(),
		signature.as_bytes(),
		canonical,
	)?;

	Ok(())
}

fn insert_signatures(
	key: &mut serde_json::Value,
	sender_id: &UserId,
	additional: impl IntoIterator<Item = (String, String)>,
) -> Result<bool> {
	let signatures = signatures_map(key)?;

	let signatures = signatures
		.entry(sender_id.to_string())
		.or_insert_with(|| serde_json::Map::new().into())
		.as_object_mut()
		.ok_or_else(|| {
			err!(Database(debug_warn!("signature data in keyid_key for a user is invalid.")))
		})?;

	let changed = additional
		.into_iter()
		.fold(false, |changed, (key_id, signature)| {
			let entry_changed = signatures
				.get(&key_id)
				.and_then(serde_json::Value::as_str)
				!= Some(&signature);

			if entry_changed {
				signatures.insert(key_id, signature.into());
			}

			changed | entry_changed
		});

	Ok(changed)
}

fn signatures_map(
	key: &mut serde_json::Value,
) -> Result<&mut serde_json::Map<String, serde_json::Value>> {
	let key = key
		.as_object_mut()
		.ok_or_else(|| err!(Database(debug_warn!("key in keyid_key is not an object."))))?;

	key.entry("signatures")
		.or_insert_with(|| serde_json::Map::new().into())
		.as_object_mut()
		.ok_or_else(|| {
			err!(Database(debug_warn!("key in keyid_key has invalid signatures field.")))
		})
}

fn replace_signatures(
	key: &mut serde_json::Value,
	sender_id: &UserId,
	(key_id, signature): (String, String),
) -> Result<bool> {
	let signatures = signatures_map(key)?;
	let replacement = serde_json::Map::from_iter([(key_id, signature.into())]);

	match signatures.get_mut(sender_id.as_str()) {
		| Some(signatures)
			if signatures
				.as_object()
				.is_some_and(|signatures| signatures == &replacement) =>
			return Ok(false),
		| Some(signatures) => *signatures = replacement.into(),
		| None => {
			signatures.insert(sender_id.to_string(), replacement.into());
		},
	}

	Ok(true)
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
#[tracing::instrument(
	name = "device_key_update"
	level = INFO_SPAN_LEVEL,
	skip_all,
	fields(%user_id),
)]
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

#[cfg(test)]
mod tests {
	use ruma::{
		signatures::{Ed25519KeyPair, KeyPair},
		user_id,
	};

	use super::*;

	fn signature_fixture() -> (String, String, Vec<u8>) {
		let der = Ed25519KeyPair::generate();
		let keypair = Ed25519KeyPair::from_der(&der, "DEVICE".to_owned())
			.expect("key pair should be generated");

		let key = serde_json::json!({
			"user_id": "@alice:example.com",
			"device_id": "DEVICE",
			"keys": { "ed25519:DEVICE": "public-key" },
		});

		let canonical = canonical_key(&key)
			.expect("signing JSON should serialize")
			.into_bytes();

		let signature = keypair.sign(&canonical).base64();
		let public_key = Base64::<Standard, _>::new(keypair.public_key()).encode();

		(public_key, signature, canonical)
	}

	#[test]
	fn verifies_canonical_signature_bytes() {
		let sender_id = user_id!("@alice:example.com");
		let key_id = <&SigningKeyId<AnyKeyName>>::try_from("ed25519:DEVICE")
			.expect("signature key ID should parse");

		let (public_key, signature, canonical) = signature_fixture();

		verify_signature(sender_id, key_id, &public_key, &signature, &canonical)
			.expect("signature should verify");
	}

	#[test]
	fn canonicalizes_stored_key_for_verification() {
		let sender_id = user_id!("@alice:example.com");
		let key_id = <&SigningKeyId<AnyKeyName>>::try_from("ed25519:DEVICE")
			.expect("signature key ID should parse");

		let der = Ed25519KeyPair::generate();
		let keypair = Ed25519KeyPair::from_der(&der, "DEVICE".to_owned())
			.expect("key pair should be generated");

		let mut stored_key = serde_json::json!({
			"user_id": sender_id,
			"device_id": "DEVICE",
			"keys": { "ed25519:DEVICE": "public-key" },
		});

		let canonical = canonical_key(&stored_key).expect("stored key should canonicalize");
		let signature = keypair.sign(canonical.as_bytes()).base64();
		let public_key = Base64::<Standard, _>::new(keypair.public_key()).encode();

		stored_key["signatures"] = serde_json::json!({
			"@bob:example.com": { "ed25519:BOB": "bob-signature" },
		});

		stored_key["unsigned"] = serde_json::json!({ "server_data": "ignored" });
		let canonical_with_metadata =
			canonical_key(&stored_key).expect("stored key with metadata should canonicalize");

		assert_eq!(canonical_with_metadata, canonical);
		verify_signature(
			sender_id,
			key_id,
			&public_key,
			&signature,
			canonical_with_metadata.as_bytes(),
		)
		.expect("signature over the stored key should verify");
	}

	#[test]
	fn rejects_signature_over_different_key() {
		let sender_id = user_id!("@alice:example.com");
		let key_id = <&SigningKeyId<AnyKeyName>>::try_from("ed25519:DEVICE")
			.expect("signature key ID should parse");

		let (public_key, signature, _) = signature_fixture();

		let error = verify_signature(sender_id, key_id, &public_key, &signature, b"{}")
			.expect_err("signature over another object should fail");

		assert!(matches!(error, Error::Signatures(_)));
	}

	#[test]
	fn rejects_malformed_signature_base64() {
		let sender_id = user_id!("@alice:example.com");
		let key_id = <&SigningKeyId<AnyKeyName>>::try_from("ed25519:DEVICE")
			.expect("signature key ID should parse");

		let (public_key, _, canonical) = signature_fixture();

		let error = verify_signature(sender_id, key_id, &public_key, "not base64?", &canonical)
			.expect_err("malformed signature base64 should fail");

		assert!(matches!(
			error,
			Error::Signatures(VerificationError::InvalidBase64Signature { .. })
		));
	}

	#[test]
	fn classifies_only_unambiguous_key_roles() {
		for mask in 0_u8..16 {
			let matches = [mask & 1 != 0, mask & 2 != 0, mask & 4 != 0, mask & 8 != 0];
			let expected = match mask {
				| 1 => Some(KeyRole::Device),
				| 2 => Some(KeyRole::CrossSigningRoot),
				| 4 => Some(KeyRole::SelfSigning),
				| 8 => Some(KeyRole::UserSigning),
				| _ => None,
			};

			assert_eq!(key_role(matches), expected, "role mask {mask:04b}");
		}
	}

	#[test]
	fn binds_key_roles_to_owner_and_row_shape() {
		let user_id = user_id!("@alice:example.com");
		let device = serde_json::json!({
			"user_id": user_id,
			"device_id": "DEVICE",
			"keys": { "ed25519:DEVICE": "device-public-key" },
		});

		let cross_signing = serde_json::json!({
			"user_id": user_id,
			"usage": ["untrusted-value"],
			"keys": { "ed25519:ROOT": "root-public-key" },
		});

		let multiple_keys = serde_json::json!({
			"user_id": user_id,
			"keys": {
				"ed25519:ROOT": "root-public-key",
				"ed25519:OTHER": "other-public-key",
			},
		});

		assert!(key_matches_role(&device, user_id, "DEVICE", KeyRole::Device));
		assert!(!key_matches_role(&device, user_id, "DEVICE", KeyRole::CrossSigningRoot));
		assert!(key_matches_role(
			&cross_signing,
			user_id,
			"root-public-key",
			KeyRole::CrossSigningRoot
		));
		assert!(!key_matches_role(
			&multiple_keys,
			user_id,
			"root-public-key",
			KeyRole::CrossSigningRoot
		));
		assert!(!key_matches_role(
			&cross_signing,
			user_id,
			"different-public-key",
			KeyRole::CrossSigningRoot
		));

		assert!(!key_matches_role(
			&cross_signing,
			user_id!("@bob:example.com"),
			"root-public-key",
			KeyRole::CrossSigningRoot
		));
	}

	#[test]
	fn accepts_only_supported_signature_scopes() {
		let roles = [
			KeyRole::Device,
			KeyRole::CrossSigningRoot,
			KeyRole::SelfSigning,
			KeyRole::UserSigning,
		];
		let cases = [false, true].into_iter().flat_map(|same_user| {
			roles.into_iter().flat_map(move |target| {
				roles
					.into_iter()
					.map(Some)
					.chain([None])
					.map(move |signer| (same_user, target, signer))
			})
		});

		for (same_user, target, signer) in cases {
			let expected = match (same_user, target, signer) {
				| (true, KeyRole::CrossSigningRoot, Some(KeyRole::Device)) =>
					SignatureAction::Write(KeyRole::Device, SignatureWrite::Merge),
				| (true, KeyRole::CrossSigningRoot, _) => SignatureAction::Reject,
				| (true, KeyRole::Device, Some(KeyRole::SelfSigning)) =>
					SignatureAction::Write(KeyRole::SelfSigning, SignatureWrite::Merge),
				| (false, KeyRole::CrossSigningRoot, Some(KeyRole::UserSigning)) =>
					SignatureAction::Write(KeyRole::UserSigning, SignatureWrite::ReplaceSender),
				| _ => SignatureAction::Ignore,
			};

			assert_eq!(
				signature_action(same_user, target, signer),
				expected,
				"same_user={same_user}, target={target:?}, signer={signer:?}"
			);
		}
	}

	#[test]
	fn replacing_signatures_bounds_key_rotation_history() {
		let sender_id = user_id!("@alice:example.com");
		let mut key = serde_json::json!({
			"signatures": {
				"@alice:example.com": { "ed25519:OLD": "old-signature" },
				"@bob:example.com": { "ed25519:BOB": "bob-signature" },
			},
		});

		let changed = replace_signatures(
			&mut key,
			sender_id,
			("ed25519:NEW".to_owned(), "new-signature".to_owned()),
		)
		.expect("signature replacement should succeed");

		assert!(changed);

		let changed = replace_signatures(
			&mut key,
			sender_id,
			("ed25519:NEW".to_owned(), "new-signature".to_owned()),
		)
		.expect("idempotent signature replacement should succeed");

		assert!(!changed);

		let sender_signatures = key["signatures"][sender_id.as_str()]
			.as_object()
			.expect("sender signatures should remain an object");

		assert_eq!(sender_signatures.len(), 1);
		assert_eq!(sender_signatures["ed25519:NEW"], "new-signature");
		assert_eq!(key["signatures"]["@bob:example.com"]["ed25519:BOB"], "bob-signature");
	}

	#[test]
	fn insert_signatures_creates_missing_map() {
		let sender_id = user_id!("@alice:example.com");
		let mut key = serde_json::json!({
			"user_id": sender_id,
			"keys": { "ed25519:ALICE": "ALICE" },
		});
		let signatures = [("ed25519:ALICE".to_owned(), "alice-signature".to_owned())];

		let changed = insert_signatures(&mut key, sender_id, signatures)
			.expect("signature insertion should succeed");

		assert!(changed);

		let signatures = [("ed25519:ALICE".to_owned(), "alice-signature".to_owned())];
		let changed = insert_signatures(&mut key, sender_id, signatures)
			.expect("idempotent signature insertion should succeed");

		assert!(!changed);

		assert_eq!(key["signatures"][sender_id.as_str()]["ed25519:ALICE"], "alice-signature");
	}

	#[test]
	fn insert_signatures_preserves_existing_signers() {
		let sender_id = user_id!("@alice:example.com");
		let mut key = serde_json::json!({
			"user_id": sender_id,
			"keys": { "ed25519:ROOT": "root-public-key" },
			"signatures": {
				"@alice:example.com": { "ed25519:OLD": "old-signature" },
				"@bob:example.com": { "ed25519:BOB": "bob-signature" },
			},
		});
		let signatures = [
			("ed25519:ALICE1".to_owned(), "alice-signature-1".to_owned()),
			("ed25519:ALICE2".to_owned(), "alice-signature-2".to_owned()),
		];

		let changed = insert_signatures(&mut key, sender_id, signatures)
			.expect("signature insertion should succeed");

		assert!(changed);

		let expected = serde_json::json!({
			"user_id": sender_id,
			"keys": { "ed25519:ROOT": "root-public-key" },
			"signatures": {
				"@alice:example.com": {
					"ed25519:OLD": "old-signature",
					"ed25519:ALICE1": "alice-signature-1",
					"ed25519:ALICE2": "alice-signature-2",
				},
				"@bob:example.com": { "ed25519:BOB": "bob-signature" },
			},
		});

		assert_eq!(key, expected);
	}

	#[test]
	fn empty_one_time_key_counts_include_signed_zero() {
		let counts = complete_one_time_key_counts(BTreeMap::new());

		assert_eq!(counts.len(), 1);
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
