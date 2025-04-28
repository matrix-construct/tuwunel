use std::{collections::BTreeMap, sync::Arc};

use futures::StreamExt;
use ruma::{
	OwnedRoomId, RoomId, UserId,
	api::client::backup::{BackupAlgorithm, KeyBackupData, RoomKeyBackup},
	serde::Raw,
};
use tuwunel_core::{
	Err, Result, err, implement,
	utils::stream::{ReadyExt, TryIgnore},
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Json, Map};

use crate::{Dep, globals};

pub struct Service {
	db: Data,
	services: Services,
}

struct Data {
	backupid_algorithm: Arc<Map>,
	backupid_etag: Arc<Map>,
	backupkeyid_backup: Arc<Map>,
}

struct Services {
	globals: Dep<globals::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				backupid_algorithm: args.db["backupid_algorithm"].clone(),
				backupid_etag: args.db["backupid_etag"].clone(),
				backupkeyid_backup: args.db["backupkeyid_backup"].clone(),
			},
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
			},
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

#[implement(Service)]
pub fn create_backup(
	&self,
	user_id: &UserId,
	backup_metadata: &Raw<BackupAlgorithm>,
) -> Result<String> {
	let version = self.services.globals.next_count()?.to_string();
	let count = self.services.globals.next_count()?;

	let key = (user_id, &version);
	self.db
		.backupid_algorithm
		.put(key, Json(backup_metadata));

	self.db.backupid_etag.put(key, count);

	Ok(version)
}

#[implement(Service)]
pub async fn delete_backup(&self, user_id: &UserId, version: &str) {
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

#[implement(Service)]
pub async fn update_backup<'a>(
	&self,
	user_id: &UserId,
	version: &'a str,
	backup_metadata: &Raw<BackupAlgorithm>,
) -> Result<&'a str> {
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

	let count = self.services.globals.next_count().unwrap();
	self.db.backupid_etag.put(key, count);
	self.db
		.backupid_algorithm
		.put_raw(key, backup_metadata.json().get());

	Ok(version)
}

#[implement(Service)]
pub async fn get_latest_backup_version(&self, user_id: &UserId) -> Result<String> {
	type Key<'a> = (&'a UserId, &'a str);

	let last_possible_key = (user_id, u64::MAX);
	self.db
		.backupid_algorithm
		.rev_keys_from(&last_possible_key)
		.ignore_err()
		.ready_take_while(|(user_id_, _): &Key<'_>| *user_id_ == user_id)
		.map(|(_, version): Key<'_>| version.to_owned())
		.next()
		.await
		.ok_or_else(|| err!(Request(NotFound("No backup versions found"))))
}

#[implement(Service)]
pub async fn get_latest_backup(
	&self,
	user_id: &UserId,
) -> Result<(String, Raw<BackupAlgorithm>)> {
	type Key<'a> = (&'a UserId, &'a str);
	type KeyVal<'a> = (Key<'a>, Raw<BackupAlgorithm>);

	let last_possible_key = (user_id, u64::MAX);
	self.db
		.backupid_algorithm
		.rev_stream_from(&last_possible_key)
		.ignore_err()
		.ready_take_while(|((user_id_, _), _): &KeyVal<'_>| *user_id_ == user_id)
		.map(|((_, version), algorithm): KeyVal<'_>| (version.to_owned(), algorithm))
		.next()
		.await
		.ok_or_else(|| err!(Request(NotFound("No backup found"))))
}

#[implement(Service)]
pub async fn get_backup(&self, user_id: &UserId, version: &str) -> Result<Raw<BackupAlgorithm>> {
	let key = (user_id, version);
	self.db
		.backupid_algorithm
		.qry(&key)
		.await
		.deserialized()
}

#[implement(Service)]
pub async fn add_key(
	&self,
	user_id: &UserId,
	version: &str,
	room_id: &RoomId,
	session_id: &str,
	key_data: &Raw<KeyBackupData>,
) -> Result<()> {
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

	let count = self.services.globals.next_count().unwrap();
	self.db.backupid_etag.put(key, count);

	let key = (user_id, version, room_id, session_id);
	self.db
		.backupkeyid_backup
		.put_raw(key, key_data.json().get());

	Ok(())
}

#[implement(Service)]
pub async fn count_keys(&self, user_id: &UserId, version: &str) -> usize {
	let prefix = (user_id, version);
	self.db
		.backupkeyid_backup
		.keys_prefix_raw(&prefix)
		.count()
		.await
}

#[implement(Service)]
pub async fn get_etag(&self, user_id: &UserId, version: &str) -> String {
	let key = (user_id, version);
	self.db
		.backupid_etag
		.qry(&key)
		.await
		.deserialized::<u64>()
		.as_ref()
		.map(ToString::to_string)
		.expect("Backup has no etag.")
}

#[implement(Service)]
pub async fn get_all(
	&self,
	user_id: &UserId,
	version: &str,
) -> BTreeMap<OwnedRoomId, RoomKeyBackup> {
	type Key<'a> = (Ignore, Ignore, &'a RoomId, &'a str);
	type KeyVal<'a> = (Key<'a>, Raw<KeyBackupData>);

	let mut rooms = BTreeMap::<OwnedRoomId, RoomKeyBackup>::new();
	let default = || RoomKeyBackup { sessions: BTreeMap::new() };

	let prefix = (user_id, version, Interfix);
	self.db
		.backupkeyid_backup
		.stream_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|((_, _, room_id, session_id), key_backup_data): KeyVal<'_>| {
			rooms
				.entry(room_id.into())
				.or_insert_with(default)
				.sessions
				.insert(session_id.into(), key_backup_data);
		})
		.await;

	rooms
}

#[implement(Service)]
pub async fn get_room(
	&self,
	user_id: &UserId,
	version: &str,
	room_id: &RoomId,
) -> BTreeMap<String, Raw<KeyBackupData>> {
	type KeyVal<'a> = ((Ignore, Ignore, Ignore, &'a str), Raw<KeyBackupData>);

	let prefix = (user_id, version, room_id, Interfix);
	self.db
		.backupkeyid_backup
		.stream_prefix(&prefix)
		.ignore_err()
		.map(|((.., session_id), key_backup_data): KeyVal<'_>| {
			(session_id.to_owned(), key_backup_data)
		})
		.collect()
		.await
}

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

#[implement(Service)]
pub async fn delete_all_keys(&self, user_id: &UserId, version: &str) {
	let key = (user_id, version, Interfix);
	self.db
		.backupkeyid_backup
		.keys_prefix_raw(&key)
		.ignore_err()
		.ready_for_each(|outdated_key| self.db.backupkeyid_backup.remove(outdated_key))
		.await;
}

#[implement(Service)]
pub async fn delete_room_keys(&self, user_id: &UserId, version: &str, room_id: &RoomId) {
	let key = (user_id, version, room_id, Interfix);
	self.db
		.backupkeyid_backup
		.keys_prefix_raw(&key)
		.ignore_err()
		.ready_for_each(|outdated_key| {
			self.db.backupkeyid_backup.remove(outdated_key);
		})
		.await;
}

#[implement(Service)]
pub async fn delete_room_key(
	&self,
	user_id: &UserId,
	version: &str,
	room_id: &RoomId,
	session_id: &str,
) {
	let key = (user_id, version, room_id, session_id);
	self.db
		.backupkeyid_backup
		.keys_prefix_raw(&key)
		.ignore_err()
		.ready_for_each(|outdated_key| {
			self.db.backupkeyid_backup.remove(outdated_key);
		})
		.await;
}
