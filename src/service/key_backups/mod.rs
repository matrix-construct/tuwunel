//! Versioned room key backup storage.
//!
//! The service stores encrypted session keys by user, backup version, room, and session. Backup
//! metadata and change tags are maintained alongside the key rows for client synchronization.

use std::{cmp::Ordering, collections::BTreeMap, module_path, sync::Arc};

use futures::{FutureExt, Stream, StreamExt, TryStreamExt, future::try_join};
use http::StatusCode;
use ruma::{
	OwnedRoomId, OwnedUserId, RoomId, UInt, UserId,
	api::{
		client::backup::{BackupAlgorithm, KeyBackupData, RoomKeyBackup},
		error::{ErrorKind, WrongRoomKeysVersionErrorData},
	},
	serde::Raw,
};
use tuwunel_core::{
	Err, Result, err, implement,
	utils::{
		MutexMap,
		stream::{ReadyExt, TryIgnore},
	},
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Json, Map};

type Key<'a> = (&'a RoomId, &'a str, &'a Raw<KeyBackupData>);
type StoredKey<'a> = (Ignore, Ignore, &'a RoomId, &'a str);
type StoredKeyVal<'a> = (StoredKey<'a>, Raw<KeyBackupData>);
type StoredRoomKeyVal<'a> = ((Ignore, Ignore, Ignore, &'a str), Raw<KeyBackupData>);
type VersionKey<'a> = (&'a UserId, &'a str);

/// Stores and mutates versioned room key backups.
///
/// Mutations take a per-user lock so concurrent update paths cannot interleave. Read operations
/// stream directly from the versioned database prefixes.
pub struct Service {
	db: Data,
	mutex: MutexMap<OwnedUserId, ()>,
	services: Arc<crate::services::OnceServices>,
}

struct Data {
	backupid_algorithm: Arc<Map>,
	backupid_etag: Arc<Map>,
	backupkeyid_backup: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				backupid_algorithm: args.db["backupid_algorithm"].clone(),
				backupid_etag: args.db["backupid_etag"].clone(),
				backupkeyid_backup: args.db["backupkeyid_backup"].clone(),
			},
			mutex: MutexMap::new(),
			services: args.services.clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(module_path!()) }
}

/// Creates an empty backup version and returns its identifier.
///
/// The version and initial change tag use separate global sequence numbers. Metadata and the change
/// tag are committed together while the user's backup lock is held.
///
/// # Panics
///
/// Panics when dispatching either global sequence number fails.
#[implement(Service)]
pub async fn create_backup(
	&self,
	user_id: &UserId,
	backup_metadata: &Raw<BackupAlgorithm>,
) -> Result<String> {
	let _backup_lock = self.mutex.lock(user_id).await;
	let version = self.services.globals.next_count();
	let count = self.services.globals.next_count();

	let version_string = version.to_string();
	let key = (user_id, &version_string);
	let mut txn = self.services.db.txn();

	txn.put(&self.db.backupid_algorithm, key, Json(backup_metadata));
	txn.put(&self.db.backupid_etag, key, *count);
	txn.execute();

	Ok(version_string)
}

/// Deletes a backup version and its stored session keys.
///
/// Metadata and the change tag are removed before the version's key rows are scanned. Missing
/// versions are accepted, and unreadable key rows are skipped during cleanup.
#[implement(Service)]
pub async fn delete_backup(&self, user_id: &UserId, version: &str) {
	let _backup_lock = self.mutex.lock(user_id).await;
	let key = (user_id, version);
	self.db.backupid_algorithm.del(key);
	self.db.backupid_etag.del(key);

	let key = (user_id, version, Interfix);

	self.db
		.backupkeyid_backup
		.keys_prefix_raw(&key)
		.ignore_err()
		.ready_for_each(|outdated_key| {
			self.db.backupkeyid_backup.remove(outdated_key);
		})
		.await;
}

/// Replaces a backup version's metadata and advances its change tag.
///
/// The version must already exist. Its metadata and fresh change tag are committed together while
/// the user's backup lock is held.
///
/// # Panics
///
/// Panics when dispatching the new global sequence number fails.
#[implement(Service)]
pub async fn update_backup<'a>(
	&self,
	user_id: &UserId,
	version: &'a str,
	backup_metadata: &Raw<BackupAlgorithm>,
) -> Result<&'a str> {
	let _backup_lock = self.mutex.lock(user_id).await;
	let key = (user_id, version);
	if self
		.db
		.backupid_algorithm
		.qry(&key)
		.await
		.is_err()
	{
		return Err!(Request(NotFound("Tried to update nonexistent backup.")));
	}

	let count = self.services.globals.next_count();
	let mut txn = self.services.db.txn();

	txn.put(&self.db.backupid_etag, key, *count);
	txn.put_raw(&self.db.backupid_algorithm, key, backup_metadata.json().get());
	txn.execute();

	Ok(version)
}

/// Returns a user's latest numeric backup version.
///
/// The greatest parseable version number is selected. Unreadable rows and nonnumeric version names
/// are skipped, and an absent result is reported as not found.
#[implement(Service)]
pub async fn get_latest_backup_version(&self, user_id: &UserId) -> Result<String> {
	let key = (user_id, Interfix);
	let latest = self
		.db
		.backupid_algorithm
		.keys_from(&key)
		.ignore_err()
		.ready_take_while(|(user_id_, _): &VersionKey<'_>| *user_id_ == user_id)
		.ready_filter_map(|(_, version): VersionKey<'_>| version.parse::<u64>().ok())
		.ready_fold(None, |latest: Option<u64>, version| {
			Some(latest.map_or(version, |latest| latest.max(version)))
		})
		.await;

	let Some(latest) = latest else {
		return Err!(Request(NotFound("No backup versions found")));
	};

	Ok(latest.to_string())
}

/// Returns a user's latest backup version and its algorithm metadata.
///
/// The version is selected by [`Self::get_latest_backup_version`]. Missing or unreadable metadata
/// is reported as a not-found request error.
#[implement(Service)]
pub async fn get_latest_backup(
	&self,
	user_id: &UserId,
) -> Result<(String, Raw<BackupAlgorithm>)> {
	let version = self.get_latest_backup_version(user_id).await?;

	let key = (user_id, version.as_str());
	self.db
		.backupid_algorithm
		.qry(&key)
		.await
		.deserialized()
		.map(|algorithm| (version, algorithm))
		.map_err(|e| err!(Request(NotFound("No backup found: {e}"))))
}

/// Loads the algorithm metadata for a backup version.
///
/// The raw value retains the stored JSON representation while constraining it to
/// [`BackupAlgorithm`].
#[implement(Service)]
pub async fn get_backup(&self, user_id: &UserId, version: &str) -> Result<Raw<BackupAlgorithm>> {
	let key = (user_id, version);
	self.db
		.backupid_algorithm
		.qry(&key)
		.await
		.deserialized()
}

/// Adds a stream of room keys to the latest backup version.
///
/// The stream is drained serially while this user's backup mutations are
/// locked. The returned count and etag describe the completed operation.
///
/// # Panics
///
/// Panics when an accepted key requires a global sequence number that cannot be allocated or
/// persisted.
#[implement(Service)]
pub async fn add_keys<'a, S>(
	&self,
	user_id: &UserId,
	version: &str,
	keys: S,
) -> Result<(usize, u64)>
where
	S: Stream<Item = Key<'a>> + Send,
{
	let _backup_lock = self.mutex.lock(user_id).await;

	self.check_backup_version(user_id, version)
		.await?;

	let key = (user_id, version);

	self.db
		.backupid_algorithm
		.qry(&key)
		.await
		.map_err(|_| err!(Request(NotFound("Tried to update nonexistent backup."))))?;

	keys.map(Ok)
		.try_for_each(async |(room_id, session_id, key_data)| {
			self.add_key(user_id, version, room_id, session_id, key_data)
				.await
		})
		.await?;

	self.get_count_etag(user_id, version).await
}

#[implement(Service)]
async fn check_backup_version(&self, user_id: &UserId, version: &str) -> Result {
	let current_version = self.get_latest_backup_version(user_id).await?;

	if current_version == version {
		return Ok(());
	}

	let status = StatusCode::BAD_REQUEST;
	let data = WrongRoomKeysVersionErrorData::new(current_version);
	let kind = ErrorKind::WrongRoomKeysVersion(data);
	let message =
		"You may only manipulate the most recently created version of the backup.".into();

	Err!(Request(kind, message, status))
}

#[implement(Service)]
async fn add_key(
	&self,
	user_id: &UserId,
	version: &str,
	room_id: &RoomId,
	session_id: &str,
	key_data: &Raw<KeyBackupData>,
) -> Result {
	// Keep the existing key unless the incoming one is preferable per MSC1219.
	let replace = match self
		.get_session(user_id, version, room_id, session_id)
		.await
	{
		| Ok(old_key) => is_better_key(&old_key, key_data)?,
		| Err(_) => true,
	};

	if !replace {
		return Ok(());
	}

	let key = (user_id, version);
	let count = self.services.globals.next_count();
	let mut txn = self.services.db.txn();

	txn.put(&self.db.backupid_etag, key, *count);

	let key = (user_id, version, room_id, session_id);

	txn.put_raw(&self.db.backupkeyid_backup, key, key_data.json().get());
	txn.execute();

	Ok(())
}

/// Reports whether a new session key should replace the stored key.
///
/// MSC1219 prefers verified keys, then lower `first_message_index`, then lower
/// `forwarded_count`. A complete tie preserves the existing key.
fn is_better_key(old: &Raw<KeyBackupData>, new: &Raw<KeyBackupData>) -> Result<bool> {
	let old_verified = old
		.get_field::<bool>("is_verified")?
		.unwrap_or_default();

	let new_verified = new
		.get_field::<bool>("is_verified")?
		.ok_or_else(|| err!(Request(BadJson("`is_verified` field should exist"))))?;

	if old_verified != new_verified {
		return Ok(new_verified);
	}

	let old_first_message_index = old
		.get_field::<UInt>("first_message_index")?
		.unwrap_or(UInt::MAX);

	let new_first_message_index = new
		.get_field::<UInt>("first_message_index")?
		.ok_or_else(|| err!(Request(BadJson("`first_message_index` field should exist"))))?;

	match new_first_message_index.cmp(&old_first_message_index) {
		| Ordering::Less => Ok(true),
		| Ordering::Greater => Ok(false),
		| Ordering::Equal => {
			let old_forwarded_count = old
				.get_field::<UInt>("forwarded_count")?
				.unwrap_or(UInt::MAX);

			let new_forwarded_count = new
				.get_field::<UInt>("forwarded_count")?
				.ok_or_else(|| err!(Request(BadJson("`forwarded_count` field should exist"))))?;

			Ok(new_forwarded_count < old_forwarded_count)
		},
	}
}

/// Reads a backup's current key count and etag.
///
/// Mutation callers keep the user lock across this method. Other callers
/// receive two independent current observations without snapshot semantics.
#[implement(Service)]
pub async fn get_count_etag(&self, user_id: &UserId, version: &str) -> Result<(usize, u64)> {
	let count = self.count_keys(user_id, version).map(Ok);
	let etag = self.get_etag(user_id, version);

	try_join(count, etag).await
}

/// Counts the session keys stored in a backup version.
///
/// Every raw key row under the user's version prefix contributes to the count.
#[implement(Service)]
pub async fn count_keys(&self, user_id: &UserId, version: &str) -> usize {
	let prefix = (user_id, version, Interfix);

	self.db
		.backupkeyid_backup
		.keys_prefix_raw(&prefix)
		.count()
		.await
}

/// Returns the current change tag for a backup version.
///
/// The tag advances whenever accepted key data or metadata changes.
#[implement(Service)]
pub async fn get_etag(&self, user_id: &UserId, version: &str) -> Result<u64> {
	let key = (user_id, version);

	self.db
		.backupid_etag
		.qry(&key)
		.await
		.deserialized::<u64>()
}

/// Loads every session key in a backup version, grouped by room.
///
/// Session identifiers map to their raw key backup data within each room. Unreadable rows are
/// omitted from the result.
#[implement(Service)]
pub async fn get_all(
	&self,
	user_id: &UserId,
	version: &str,
) -> BTreeMap<OwnedRoomId, RoomKeyBackup> {
	let default = || RoomKeyBackup { sessions: BTreeMap::new() };
	let prefix = (user_id, version, Interfix);

	self.db
		.backupkeyid_backup
		.stream_prefix(&prefix)
		.ignore_err()
		.ready_fold(BTreeMap::new(), |mut rooms, row: StoredKeyVal<'_>| {
			let ((_, _, room_id, session_id), key_backup_data) = row;

			rooms
				.entry(room_id.into())
				.or_insert_with(default)
				.sessions
				.insert(session_id.into(), key_backup_data);

			rooms
		})
		.await
}

/// Loads every backed-up session key for one room.
///
/// The returned map is keyed by session ID and retains each value as raw key backup data.
/// Unreadable rows are omitted.
#[implement(Service)]
pub async fn get_room(
	&self,
	user_id: &UserId,
	version: &str,
	room_id: &RoomId,
) -> BTreeMap<String, Raw<KeyBackupData>> {
	let prefix = (user_id, version, room_id, Interfix);

	self.db
		.backupkeyid_backup
		.stream_prefix(&prefix)
		.ignore_err()
		.map(|((.., session_id), key_backup_data): StoredRoomKeyVal<'_>| {
			(session_id.to_owned(), key_backup_data)
		})
		.collect()
		.await
}

/// Loads one backed-up session key.
///
/// The key is selected by user, backup version, room, and session ID. The stored JSON is returned
/// without eagerly deserializing the key data.
#[implement(Service)]
pub async fn get_session(
	&self,
	user_id: &UserId,
	version: &str,
	room_id: &RoomId,
	session_id: &str,
) -> Result<Raw<KeyBackupData>> {
	let key = (user_id, version, room_id, session_id);

	self.db
		.backupkeyid_backup
		.qry(&key)
		.await
		.deserialized()
}

/// Deletes session keys from a backup version.
///
/// The version must exist. Rows whose scan reports an error are skipped. Deletion runs under the
/// user's backup lock, advances the change tag, and reports a zero remaining count with the new tag.
///
/// # Panics
///
/// Panics when dispatching the new change tag fails.
#[implement(Service)]
pub async fn delete_all_keys(&self, user_id: &UserId, version: &str) -> Result<(usize, u64)> {
	let _backup_lock = self.mutex.lock(user_id).await;

	self.check_backup_exists(user_id, version).await?;

	let key = (user_id, version, Interfix);
	self.db
		.backupkeyid_backup
		.keys_prefix_raw(&key)
		.ignore_err()
		.ready_for_each(|outdated_key| self.db.backupkeyid_backup.remove(outdated_key))
		.await;

	let etag = self.bump_etag(user_id, version);

	Ok((0, etag))
}

#[implement(Service)]
async fn check_backup_exists(&self, user_id: &UserId, version: &str) -> Result {
	let algorithm = self
		.get_backup(user_id, version)
		.map(|result| result.map(drop));

	let etag = self
		.get_etag(user_id, version)
		.map(|result| result.map(drop));

	try_join(algorithm, etag).await.map(drop)
}

#[implement(Service)]
fn bump_etag(&self, user_id: &UserId, version: &str) -> u64 {
	let etag = self.services.globals.next_count();

	self.db
		.backupid_etag
		.put((user_id, version), *etag);

	*etag
}

/// Deletes session keys for one room in a backup version.
///
/// The version must exist. Rows whose scan reports an error are skipped. Deletion runs under the
/// user's backup lock, advances the change tag, and returns the remaining key count from the
/// exact-version scan.
///
/// # Panics
///
/// Panics when dispatching the new change tag fails.
#[implement(Service)]
pub async fn delete_room_keys(
	&self,
	user_id: &UserId,
	version: &str,
	room_id: &RoomId,
) -> Result<(usize, u64)> {
	let _backup_lock = self.mutex.lock(user_id).await;

	self.check_backup_exists(user_id, version).await?;

	let key = (user_id, version, room_id, Interfix);

	self.db
		.backupkeyid_backup
		.keys_prefix_raw(&key)
		.ignore_err()
		.ready_for_each(|outdated_key| self.db.backupkeyid_backup.remove(outdated_key))
		.await;

	let etag = self.bump_etag(user_id, version);
	let count = self.count_keys(user_id, version).await;

	Ok((count, etag))
}

/// Deletes one session key from a backup version.
///
/// The version must exist. The change tag advances even when the selected key was absent, and the
/// returned count covers every remaining key in the version.
///
/// # Panics
///
/// Panics when dispatching the new change tag fails.
#[implement(Service)]
pub async fn delete_room_key(
	&self,
	user_id: &UserId,
	version: &str,
	room_id: &RoomId,
	session_id: &str,
) -> Result<(usize, u64)> {
	let _backup_lock = self.mutex.lock(user_id).await;

	self.check_backup_exists(user_id, version).await?;

	self.db
		.backupkeyid_backup
		.del((user_id, version, room_id, session_id));

	let etag = self.bump_etag(user_id, version);
	let count = self.count_keys(user_id, version).await;

	Ok((count, etag))
}
