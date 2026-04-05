use std::{fs, io::Cursor, sync::Arc, time::Duration};

use bytes::Bytes;
use reqwest::Client;
use tar::Archive;
use tuwunel_core::{Err, Result, Server, err, implement, info, warn};
use url::Url;

pub async fn maybe_bootstrap_checkpoint(server: &Arc<Server>) -> Result {
	let Some(primary_url) = server.config.rocksdb_primary_url.as_ref() else {
		return Ok(());
	};

	bootstrap_checkpoint(server, primary_url).await
}

#[tracing::instrument(
	level = "info",
	ret,
	skip_all,
	fields(url = primary_url.as_str())
)]
async fn bootstrap_checkpoint(server: &Arc<Server>, primary_url: &Url) -> Result {
	let db_path = server.config.database_path.clone();
	let sidecar = db_path
		.parent()
		.unwrap_or(&db_path)
		.join("_replication_needs_bootstrap");

	let current_file = db_path.join("CURRENT");
	let needs_bootstrap = sidecar.exists()
		|| !current_file.exists()
		|| fs::metadata(&current_file)
			.as_ref()
			.map(fs::Metadata::len)
			.unwrap_or(0)
			.eq(&0);

	if !needs_bootstrap {
		return Ok(());
	}

	let token = server
		.config
		.rocksdb_replication_token
		.as_deref()
		.unwrap_or_default();

	info!("Pre-open bootstrap: downloading checkpoint from {primary_url}");

	let client = Client::builder()
		.connect_timeout(Duration::from_secs(10))
		.build()
		.map_err(|e| err!(Database("Failed to build HTTP client: {e}")))?;

	let resp = client
		.get(primary_url.join("_tuwunel/cluster/checkpoint")?)
		.header("x-tuwunel-replication-token", token)
		.send()
		.await
		.map_err(|e| err!(Database("Checkpoint request failed: {e}")))?;

	if !resp.status().is_success() {
		return Err!(Database("Primary returned {} for checkpoint", resp.status()));
	}

	let seq: u64 = resp
		.headers()
		.get("x-tuwunel-checkpoint-sequence")
		.and_then(|v| v.to_str().ok())
		.and_then(|s| s.parse().ok())
		.unwrap_or(0);

	let tar_bytes: Bytes = resp
		.bytes()
		.await
		.map_err(|e| err!(Database("Reading checkpoint body: {e}")))?;

	let parent = db_path.parent().unwrap_or(&db_path);
	let staging = parent.join("_replication_staging");
	let backup = parent.join("_replication_backup");

	if staging.exists() {
		fs::remove_dir_all(&staging).map_err(|e| err!(Database("Removing staging dir: {e}")))?;
	}

	fs::create_dir_all(&staging).map_err(|e| err!(Database("Creating staging dir: {e}")))?;

	let cursor = Cursor::new(&*tar_bytes);
	let mut archive = Archive::new(cursor);
	archive
		.unpack(&staging)
		.map_err(|e| err!(Database("Unpacking checkpoint: {e}")))?;

	let checkpoint_src = staging.join("checkpoint");

	if backup.exists() {
		fs::remove_dir_all(&backup).map_err(|e| err!(Database("Removing backup: {e}")))?;
	}

	if db_path.exists() {
		fs::rename(&db_path, &backup).map_err(|e| err!(Database("Moving db to backup: {e}")))?;
	}

	fs::rename(&checkpoint_src, &db_path)
		.map_err(|e| err!(Database("Moving checkpoint to db_path: {e}")))?;

	fs::remove_dir_all(&staging).ok();

	fs::write(&sidecar, seq.to_string())
		.map_err(|e| err!(Database("Writing bootstrap sidecar: {e}")))?;

	info!("Pre-open bootstrap complete; resume_seq = {seq}. RocksDB will open clean checkpoint.");

	Ok(())
}

#[implement(super::Service)]
#[tracing::instrument(level = "info", skip_all)]
pub(super) fn bootstrap_resume_seq(&self) -> Result {
	let db_path = self.server.config.database_path.clone();
	let parent = db_path.parent().unwrap_or(&db_path);
	let sidecar = parent.join("_replication_needs_bootstrap");

	if sidecar.exists() {
		let seq_str = fs::read_to_string(&sidecar)
			.map_err(|e| err!(Database("Reading bootstrap sidecar: {e}")))?;

		let seq: u64 = seq_str
			.trim()
			.parse()
			.map_err(|e| err!(Database("Parsing bootstrap sidecar: {e}")))?;

		self.set_resume_seq(seq)?;
		fs::remove_file(&sidecar)
			.map_err(|e| err!(Database("Removing bootstrap sidecar: {e}")))?;

		info!("Bootstrap complete via pre-open path; resume_seq = {seq}");
	} else {
		let db_seq = self.db.engine.current_sequence();
		if db_seq > 0 {
			warn!(
				"resume_seq == 0 but database has sequence {db_seq} (was primary). Attempting \
				 WAL resume from {db_seq}."
			);

			self.set_resume_seq(db_seq)?;
		} else {
			fs::write(&sidecar, "0")
				.map_err(|e| err!(Database("Writing bootstrap trigger: {e}")))?;

			return Err!(Database(warn!(
				"Empty database; restarting for pre-open checkpoint bootstrap"
			)));
		}
	}

	Ok(())
}
