//! Secondary replication service.
//!
//! When `config.rocksdb_primary_url` is set, this service continuously tails
//! the primary's WAL stream and applies incoming batches to the local
//! (secondary) database. On startup it bootstraps from a checkpoint if no
//! resume cursor is persisted. On failover the secondary can be promoted to
//! primary by calling `POST /_tuwunel/replication/promote`. A promoted node
//! (or any standalone primary) can be demoted back to a secondary by calling
//! `POST /_tuwunel/replication/demote` with a new primary URL.
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
//!   -> on promote(): enter standby loop, instance becomes standalone primary
//!   -> on demote(url): exit standby, bootstrap from new primary, resume stream
//! ```

use std::{
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::{Notify, RwLock};
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
	/// Set to true when `promote()` is called; worker enters standby mode.
	promoted: AtomicBool,
	/// Wakes any blocking select in the streaming loop immediately on
	/// promotion.
	promote_notify: Notify,
	/// Runtime-overridden primary URL set by `demote()`. Takes precedence over
	/// `config.rocksdb_primary_url` when set.
	dynamic_primary_url: RwLock<Option<String>>,
	/// Wakes the standby loop immediately when `demote()` is called.
	demote_notify: Notify,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &Args<'_>) -> Result<Arc<Self>> {
		let client = reqwest::Client::builder()
			.connect_timeout(Duration::from_secs(10))
			.build()
			.map_err(|e| err!(Database("Failed to build replication HTTP client: {e}")))?;

		Ok(Arc::new(Self {
			db: args.db.clone(),
			server: args.server.clone(),
			client,
			promoted: AtomicBool::new(false),
			promote_notify: Notify::new(),
			dynamic_primary_url: RwLock::new(None),
			demote_notify: Notify::new(),
		}))
	}

	/// Worker loop: manages transitions between secondary (replicating),
	/// standby (promoted primary), and secondary-again (after demote).
	///
	/// The worker runs until server shutdown regardless of role transitions so
	/// that `demote()` can restart replication without restarting the process.
	async fn worker(self: Arc<Self>) -> Result {
		if self.db.is_secondary() {
			// RocksDB opened in native secondary mode (read-only) -- WAL streaming
			// replication requires a writable database. Operator should use either
			// rocksdb_secondary OR rocksdb_primary_url, not both.
			warn!(
				"rocksdb_primary_url is set but database is in RocksDB native secondary mode \
				 (read-only); WAL streaming replication requires a writable database. \
				 Replication worker will not run."
			);
			return Ok(());
		}

		loop {
			if !self.server.running() {
				return Ok(());
			}

			// Resolve effective primary URL: dynamic (set by demote) takes
			// precedence over the static config value.
			let primary_url = {
				let dynamic = self.dynamic_primary_url.read().await;
				dynamic
					.clone()
					.or_else(|| self.server.config.rocksdb_primary_url.clone())
			};

			// If no primary URL is configured, wait for a demote() call (which
			// sets a dynamic URL) or server shutdown. This keeps the worker alive
			// on nodes that start as standalone primaries so they can be demoted
			// without a process restart.
			let Some(primary_url) = primary_url else {
				tokio::select! {
					() = self.server.until_shutdown() => return Ok(()),
					() = self.demote_notify.notified() => continue,
				}
			};

			// If currently promoted, enter standby and wait for a demote signal.
			if self.promoted.load(Ordering::Acquire) {
				info!("In standalone primary mode; waiting for demote or shutdown.");
				tokio::select! {
					() = self.server.until_shutdown() => return Ok(()),
					() = self.demote_notify.notified() => {
						info!("Demote received; resuming replication from {primary_url}");
						continue;
					},
				}
			}

			// Bootstrap if no cursor is saved (first run or after demote reset).
			let resume_seq = self.db.get_replication_resume_seq()?;
			if resume_seq == 0 {
				info!("No resume cursor found; bootstrapping from primary checkpoint");
				self.bootstrap(&primary_url).await?;
			}

			info!("Replication worker starting; primary = {primary_url}");

			let mut backoff_ms = BACKOFF_MIN_MS;

			while self.server.running() && !self.promoted.load(Ordering::Acquire) {
				match self.run_stream(&primary_url).await {
					| Ok(()) => {
						// run_stream returns Ok on clean shutdown or promotion.
						if self.promoted.load(Ordering::Acquire) {
							break; // fall through to standby at top of outer loop
						}
						return Ok(()); // server is stopping
					},
					| Err(ref e) if is_wal_gap_error(e) => {
						error!(
							"WAL gap: primary no longer has WAL history for our resume \
							 position. Manual intervention required: stop this secondary, \
							 restore a fresh checkpoint over the database directory, then \
							 restart. Stopping replication worker."
						);
						return Err(err!(Database(
							"WAL gap; manual checkpoint restore required"
						)));
					},
					| Err(ref e) => {
						if self.promoted.load(Ordering::Acquire) {
							break;
						}
						error!("Replication stream error: {e}; reconnecting in {backoff_ms}ms");
					},
				}

				// Exponential backoff with cap — also wakes on promotion or shutdown.
				tokio::select! {
					() = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {},
					() = self.server.until_shutdown() => return Ok(()),
					() = self.promote_notify.notified() => break,
				}
				#[allow(clippy::arithmetic_side_effects)]
				{
					backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
				}
			}
		}
	}

	fn name(&self) -> &str { make_name(std::module_path!()) }
}

impl Service {
	/// Promote this secondary to a standalone primary.
	///
	/// Stops the replication worker immediately. The caller is responsible for
	/// updating the VIP / load balancer to route client traffic to this node.
	pub fn promote(&self) {
		self.promoted.store(true, Ordering::Release);
		self.promote_notify.notify_waiters();
		info!("Promotion requested; stopping replication worker.");
	}

	/// Returns true if this instance has been promoted to primary.
	pub fn is_promoted(&self) -> bool { self.promoted.load(Ordering::Acquire) }

	/// Demote this promoted primary back to a secondary replicating from
	/// `new_primary_url`.
	///
	/// Resets the resume cursor so the worker performs a clean checkpoint
	/// bootstrap from the new primary (whose WAL history will differ from
	/// ours). The caller is responsible for ensuring the VIP / load balancer
	/// has been updated to route writes to the new primary before calling this.
	///
	/// Returns `Err` if the instance is not currently promoted.
	pub async fn demote(&self, new_primary_url: String) -> Result<()> {
		if !self.promoted.load(Ordering::Acquire) {
			return Err(err!(Database(
				"This instance is not currently promoted; cannot demote."
			)));
		}

		// Reset cursor so the worker bootstraps a fresh checkpoint from the new
		// primary rather than trying to resume from our own WAL position.
		self.db.set_replication_resume_seq(0)?;

		// Store the new primary URL and clear the promoted flag before notifying
		// the worker so it sees a consistent state on wake-up.
		*self.dynamic_primary_url.write().await = Some(new_primary_url.clone());
		self.promoted.store(false, Ordering::Release);
		self.demote_notify.notify_waiters();

		info!("Demotion requested; will replicate from {new_primary_url}");
		Ok(())
	}

	/// Stream WAL frames from the primary until disconnect, promotion, or
	/// error.
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
			return Err(err!(Database("Primary returned {} for WAL stream", resp.status())));
		}

		info!("WAL stream connected; starting from seq {resume_seq}");

		let mut byte_stream = resp.bytes_stream();
		let mut buf: Vec<u8> = Vec::new();

		while self.server.running() && !self.promoted.load(Ordering::Acquire) {
			tokio::select! {
				chunk = byte_stream.next() => {
					let Some(chunk) = chunk else {
						return Err(err!(Database("Primary closed WAL stream")));
					};
					let chunk = chunk.map_err(|e| err!(Database("WAL stream read: {e}")))?;
					buf.extend_from_slice(&chunk);
					self.drain_frames(&mut buf)?;
				},
				() = self.server.until_shutdown() => return Ok(()),
				() = self.promote_notify.notified() => return Ok(()),
			}
		}

		Ok(())
	}

	/// Parse and apply as many complete frames as possible from `buf`.
	fn drain_frames(&self, buf: &mut Vec<u8>) -> Result {
		let mut offset = 0;
		while let Ok((frame, consumed)) = WalFrame::decode(&buf[offset..]) {
			self.apply_frame(&frame)?;
			#[allow(clippy::arithmetic_side_effects)]
			{
				offset += consumed;
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

		let next = frame.next_resume_seq();
		if next > 0 {
			self.db.set_replication_resume_seq(next)?;
		}
		Ok(())
	}

	/// Full sync: download a checkpoint tar from the primary and restore it.
	async fn bootstrap(&self, primary_url: &str) -> Result {
		let url = format!("{primary_url}/_tuwunel/replication/checkpoint");
		info!("Downloading checkpoint from {url}");

		let resp = self
			.authed_get(&url)
			.await
			.map_err(|e| err!(Database("GET {url}: {e}")))?;

		if !resp.status().is_success() {
			return Err(err!(Database("Primary returned {} for checkpoint", resp.status())));
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

		if staging.exists() {
			std::fs::remove_dir_all(&staging)
				.map_err(|e| err!(Database("Removing old staging dir: {e}")))?;
		}
		std::fs::create_dir_all(&staging)
			.map_err(|e| err!(Database("Creating staging dir: {e}")))?;

		let cursor = std::io::Cursor::new(&*tar_bytes);
		let mut archive = tar::Archive::new(cursor);
		archive
			.unpack(&staging)
			.map_err(|e| err!(Database("Unpacking checkpoint tar: {e}")))?;

		let checkpoint_src = staging.join("checkpoint");

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

		let _: std::io::Result<()> = std::fs::remove_dir_all(&staging);

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
