//! Secondary replication service.
//!
//! When `config.rocksdb_primary_url` is set, this service continuously tails
//! the primary's WAL stream and applies incoming batches to the local
//! (secondary) database. On startup it bootstraps from a checkpoint if no
//! resume cursor is persisted. On failover the secondary can be promoted to
//! primary by restarting without `rocksdb_primary_url`.
//!
//! ## Normal operation
//!
//! ```text
//! startup
//!   -> load resume_seq from replication_meta CF
//!   -> if resume_seq == 0: bootstrap (GET /checkpoint, restore, set resume_seq)
//!   -> connect to GET /wal?since=<resume_seq>
//!   -> stream: for each frame apply batch, advance resume_seq, persist cursor
//!   -> on disconnect / error: exponential backoff, reconnect
//!   -> on 410 Gone (WAL gap): stop with error (manual restore required)
//! ```

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use tuwunel_core::{Result, err, error, info, warn};
use tuwunel_database::{Database, WalFrame, is_wal_gap_error};

use crate::service::{Args, make_name};

/// Minimum retry delay after a transient connection error.
const BACKOFF_MIN_MS: u64 = 500;
/// Maximum retry delay (caps the exponential backoff).
const BACKOFF_MAX_MS: u64 = 30_000;

pub struct Service {
	db: Arc<Database>,
	server: Arc<tuwunel_core::Server>,
	/// HTTP client used for all primary connections.
	client: reqwest::Client,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &Args<'_>) -> Result<Arc<Self>> {
		let client = reqwest::Client::builder()
			.timeout(Duration::from_secs(60))
			.build()
			.map_err(|e| err!(Database("Failed to build replication HTTP client: {e}")))?;

		Ok(Arc::new(Self {
			db: args.db.clone(),
			server: args.server.clone(),
			client,
		}))
	}

	/// Worker loop: only runs if `rocksdb_primary_url` is configured.
	async fn worker(self: Arc<Self>) -> Result {
		let Some(primary_url) = self.server.config.rocksdb_primary_url.clone() else {
			// Not a secondary -- nothing to do.
			return Ok(());
		};

		if self.db.is_secondary() {
			// RocksDB opened in native secondary mode (read-only) -- WAL streaming
			// replication requires a writable database. Operator should use either
			// rocksdb_secondary OR rocksdb_primary_url, not both.
			warn!(
				"rocksdb_primary_url is set but database is in RocksDB native secondary \
				 mode (read-only); WAL streaming replication requires a writable database. \
				 Replication worker will not run."
			);
			return Ok(());
		}

		// Bootstrap if no cursor is saved (first run after checkpoint restore).
		let resume_seq = self.db.get_replication_resume_seq()?;
		if resume_seq == 0 {
			info!("No resume cursor found; bootstrapping from primary checkpoint");
			self.bootstrap(&primary_url).await?;
		}

		info!("Replication worker starting; primary = {primary_url}");

		let mut backoff_ms = BACKOFF_MIN_MS;

		while self.server.running() {
			match self.run_stream(&primary_url).await {
				| Ok(()) => {
					// Clean shutdown (server stopping).
					return Ok(());
				},
				| Err(ref e) if is_wal_gap_error(e) => {
					// Live-replacing an open RocksDB directory is not safe.
					// The admin must stop this node, restore a fresh checkpoint
					// over `config.database_path`, then restart.
					error!(
						"WAL gap: primary no longer has WAL history for our resume \
						 position. Manual intervention required: stop this secondary, \
						 restore a fresh checkpoint over the database directory, then \
						 restart. Stopping replication worker."
					);
					return Err(err!(Database("WAL gap; manual checkpoint restore required")));
				},
				| Err(ref e) => {
					error!("Replication stream error: {e}; reconnecting in {backoff_ms}ms");
				},
			}

			// Exponential backoff with cap.
			tokio::select! {
				_ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {},
				() = self.server.until_shutdown() => return Ok(()),
			}
			backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
		}

		Ok(())
	}

	fn name(&self) -> &str { make_name(std::module_path!()) }
}

impl Service {
	/// Stream WAL frames from the primary until disconnect or error.
	///
	/// Returns `Err` wrapping a WAL-gap error when the primary responds 410.
	async fn run_stream(&self, primary_url: &str) -> Result {
		let resume_seq = self.db.get_replication_resume_seq()?;
		let url = format!("{primary_url}/_tuwunel/replication/wal?since={resume_seq}");

		let resp = self
			.authed_get(&url)
			.await
			.map_err(|e| err!(Database("GET {url}: {e}")))?;

		if resp.status() == reqwest::StatusCode::GONE {
			return Err(err!(Database("WAL gap: 410 Gone from primary")));
		}

		if !resp.status().is_success() {
			return Err(err!(Database(
				"Primary returned {} for WAL stream",
				resp.status()
			)));
		}

		info!("WAL stream connected; starting from seq {resume_seq}");

		let mut byte_stream = resp.bytes_stream();
		let mut buf: Vec<u8> = Vec::new();

		while self.server.running() {
			tokio::select! {
				chunk = byte_stream.next() => {
					let Some(chunk) = chunk else {
						// Primary closed the connection gracefully.
						return Err(err!(Database("Primary closed WAL stream")));
					};
					let chunk = chunk.map_err(|e| err!(Database("WAL stream read: {e}")))?;
					buf.extend_from_slice(&chunk);
					self.drain_frames(&mut buf)?;
				},
				() = self.server.until_shutdown() => return Ok(()),
			}
		}

		Ok(())
	}

	/// Parse and apply as many complete frames as possible from `buf`.
	///
	/// Advances the buffer in-place, leaving any incomplete trailing bytes.
	fn drain_frames(&self, buf: &mut Vec<u8>) -> Result {
		let mut offset = 0;
		loop {
			match WalFrame::decode(&buf[offset..]) {
				| Ok((frame, consumed)) => {
					self.apply_frame(&frame)?;
					offset += consumed;
				},
				| Err(_) => break, // incomplete frame -- wait for more bytes
			}
		}
		buf.drain(..offset);
		Ok(())
	}

	/// Apply a single frame to the local database.
	fn apply_frame(&self, frame: &WalFrame) -> Result {
		use tuwunel_database::FRAME_TYPE_DATA;

		if frame.frame_type == FRAME_TYPE_DATA && !frame.batch_data.is_empty() {
			self.db.write_raw_batch(&frame.batch_data)?;
		}

		// Advance cursor. next_resume_seq() returns sequence unchanged for heartbeats.
		let next = frame.next_resume_seq();
		if next > 0 {
			self.db.set_replication_resume_seq(next)?;
		}
		Ok(())
	}

	/// Full sync: download a checkpoint tar from the primary, extract it over
	/// the local database directory, and set the resume cursor.
	///
	/// This is intended for the initial setup case (before the database has
	/// processed any traffic on the secondary). Live replacement of an open
	/// database is NOT supported -- restart the service after a manual restore.
	async fn bootstrap(&self, primary_url: &str) -> Result {
		let url = format!("{primary_url}/_tuwunel/replication/checkpoint");
		info!("Downloading checkpoint from {url}");

		let resp = self
			.authed_get(&url)
			.await
			.map_err(|e| err!(Database("GET {url}: {e}")))?;

		if !resp.status().is_success() {
			return Err(err!(Database(
				"Primary returned {} for checkpoint",
				resp.status()
			)));
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

		let db_path = self.server.config.database_path.clone();
		let parent = db_path.parent().unwrap_or(&db_path).to_owned();
		let staging = parent.join("_replication_staging");
		let backup = parent.join("_replication_backup");

		// Clean up any leftover staging dir.
		if staging.exists() {
			std::fs::remove_dir_all(&staging)
				.map_err(|e| err!(Database("Removing old staging dir: {e}")))?;
		}
		std::fs::create_dir_all(&staging)
			.map_err(|e| err!(Database("Creating staging dir: {e}")))?;

		// Unpack tar archive into staging/.
		let cursor = std::io::Cursor::new(&tar_bytes[..]);
		let mut archive = tar::Archive::new(cursor);
		archive
			.unpack(&staging)
			.map_err(|e| err!(Database("Unpacking checkpoint tar: {e}")))?;

		// Archive contains a single top-level `checkpoint/` directory.
		let checkpoint_src = staging.join("checkpoint");

		// Rotate: backup old db, move checkpoint into place.
		if backup.exists() {
			std::fs::remove_dir_all(&backup)
				.map_err(|e| err!(Database("Removing old backup: {e}")))?;
		}
		if db_path.exists() {
			std::fs::rename(&db_path, &backup)
				.map_err(|e| err!(Database("Moving db to backup: {e}")))?;
		}
		std::fs::rename(&checkpoint_src, &db_path)
			.map_err(|e| err!(Database("Moving checkpoint to db_path: {e}")))?;

		let _ = std::fs::remove_dir_all(&staging);

		// Persist the sequence the primary told us.
		self.db.set_replication_resume_seq(seq)?;

		info!("Checkpoint bootstrap complete; resume_seq = {seq}");
		Ok(())
	}

	/// Send an authenticated GET request to the primary.
	async fn authed_get(&self, url: &str) -> reqwest::Result<reqwest::Response> {
		let mut req = self.client.get(url);
		if let Some(ref token) = self.server.config.rocksdb_replication_token {
			req = req.header("x-tuwunel-replication-token", token.as_str());
		}
		req.send().await
	}
}
