use std::{
	collections::HashSet,
	ffi::{OsStr, OsString},
	fs::{self},
	path::PathBuf,
	sync::Arc,
	time::Instant,
};

use tuwunel_core::{
	Config, Result, debug, debug_info, debug_warn, error,
	error::inspect_debug_log,
	info,
	utils::{ReadyExt, stream::TryIgnore},
	warn,
};

use crate::Services;

/// Migrates a media directory from legacy base64 file names to sha2 file names.
/// All errors are fatal. Upon success the database is keyed to not perform this
/// again.
pub(crate) async fn migrate_sha256_media(services: &Services) -> Result {
	let db = &services.db;
	let config = &services.server.config;

	warn!("Migrating legacy base64 file names to sha256 file names");
	let mediaid_file = &db["mediaid_file"];

	// Move old media files to new names
	let mut changes = Vec::<(PathBuf, PathBuf)>::new();
	mediaid_file
		.raw_keys()
		.ignore_err()
		.ready_for_each(|key| {
			let old = services.media.get_media_path_b64(key);
			let new = services.media.get_media_path_sha256(key);
			debug!(?key, ?old, ?new, num = changes.len(), "change");
			changes.push((old, new));
		})
		.await;

	// move the file to the new location
	for (old_path, path) in changes {
		if old_path.exists() {
			tokio::fs::rename(&old_path, &path).await?;
			if config.media_compat_file_link {
				tokio::fs::symlink(&path, &old_path).await?;
			}
		}
	}

	db["global"].insert(b"feat_sha256_media", []);
	info!("Finished applying sha256_media");
	Ok(())
}

/// Check is run on startup for prior-migrated media directories. This handles:
/// - Going back and forth to non-sha256 legacy binaries (e.g. upstream).
/// - Deletion of artifacts in the media directory which will then fall out of
///   sync with the database.
pub(crate) async fn checkup_sha256_media(services: &Services) -> Result {
	use crate::media::encode_key;

	debug!("Checking integrity of media directory");
	let db = &services.db;
	let media = &services.media;
	let config = &services.server.config;
	let mediaid_file = &db["mediaid_file"];
	let mediaid_user = &db["mediaid_user"];
	let dbs = (mediaid_file, mediaid_user);
	let timer = Instant::now();

	let dir = media.get_media_dir();
	let files: HashSet<OsString> = fs::read_dir(dir)
		.inspect_err(inspect_debug_log)
		.into_iter()
		.flatten()
		.filter_map(|ent| ent.map_or(None, |ent| Some(ent.path().into_os_string())))
		.collect();

	for key in media.db.get_all_media_keys().await {
		let new_path = media.get_media_path_sha256(&key).into_os_string();
		let old_path = media.get_media_path_b64(&key).into_os_string();
		if let Err(e) = handle_media_check(&dbs, config, &files, &key, &new_path, &old_path).await
		{
			error!(
				media_id = ?encode_key(&key), ?new_path, ?old_path,
				"Failed to resolve media check failure: {e}"
			);
		}
	}

	debug_info!(
		elapsed = ?timer.elapsed(),
		"Finished checking media directory"
	);

	Ok(())
}

async fn handle_media_check(
	dbs: &(&Arc<tuwunel_database::Map>, &Arc<tuwunel_database::Map>),
	config: &Config,
	files: &HashSet<OsString>,
	key: &[u8],
	new_path: &OsStr,
	old_path: &OsStr,
) -> Result {
	use crate::media::encode_key;

	let (mediaid_file, mediaid_user) = dbs;

	let new_exists = files.contains(new_path);
	let old_exists = files.contains(old_path);
	let old_is_symlink = || async {
		tokio::fs::symlink_metadata(old_path)
			.await
			.is_ok_and(|md| md.is_symlink())
	};

	if config.prune_missing_media && !old_exists && !new_exists {
		error!(
			media_id = ?encode_key(key), ?new_path, ?old_path,
			"Media is missing at all paths. Removing from database..."
		);

		mediaid_file.remove(key);
		mediaid_user.remove(key);
	}

	if config.media_compat_file_link && !old_exists && new_exists {
		debug_warn!(
			media_id = ?encode_key(key), ?new_path, ?old_path,
			"Media found but missing legacy link. Fixing..."
		);

		tokio::fs::symlink(&new_path, &old_path).await?;
	}

	if config.media_compat_file_link && !new_exists && old_exists {
		debug_warn!(
			media_id = ?encode_key(key), ?new_path, ?old_path,
			"Legacy media found without sha256 migration. Fixing..."
		);

		debug_assert!(
			old_is_symlink().await,
			"Legacy media not expected to be a symlink without an existing sha256 migration."
		);

		tokio::fs::rename(&old_path, &new_path).await?;
		tokio::fs::symlink(&new_path, &old_path).await?;
	}

	if !config.media_compat_file_link && old_exists && old_is_symlink().await {
		debug_warn!(
			media_id = ?encode_key(key), ?new_path, ?old_path,
			"Legacy link found but compat disabled. Cleansing symlink..."
		);

		debug_assert!(
			new_exists,
			"sha256 migration into new file expected prior to cleaning legacy symlink here."
		);

		tokio::fs::remove_file(&old_path).await?;
	}

	Ok(())
}
