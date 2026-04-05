//! Multi-instance clustering service.
//!
//! When `config.rocksdb_primary_url` is set, this service continuously tails
//! the primary's WAL stream and applies incoming batches to the local
//! (secondary) database. On startup it bootstraps from a checkpoint if no
//! resume cursor is persisted. On failover the secondary can be promoted to
//! primary by calling `POST /_tuwunel/cluster/promote`. A promoted node
//! (or any standalone primary) can be demoted back to a secondary by calling
//! `POST /_tuwunel/cluster/demote` with a new primary URL.
//!
//! ## Normal operation
//!
//! ```text
//! startup
//!   -> load resume_seq from global CF
//!   -> if resume_seq == 0: bootstrap (GET /checkpoint, restore, set resume_seq)
//!   -> connect to GET /wal?since=<resume_seq>
//!   -> stream: for each frame apply batch, advance resume_seq, persist cursor
//!   -> on disconnect / error: exponential backoff, reconnect
//!   -> on 410 Gone (WAL gap): stop with error (manual restore required)
//!   -> on promote(): enter standby loop, instance becomes standalone primary
//!   -> on demote(url): exit standby, bootstrap from new primary, resume stream
//! ```

mod bootstrap;
mod sync;

use std::{
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use async_trait::async_trait;
use tokio::{
	sync::{Notify, RwLock},
	time::sleep,
};
use tuwunel_core::{Err, Result, Server, debug, err, error, implement, info, warn};
use tuwunel_database::{Database, Map, is_wal_gap_error};
use url::Url;

pub use self::bootstrap::maybe_bootstrap_checkpoint;
use crate::{
	service::{Args, make_name},
	services::OnceServices,
};

pub struct Service {
	db: Arc<Database>,
	server: Arc<Server>,
	services: Arc<OnceServices>,
	global: Arc<Map>,

	/// HTTP client used for all primary connections.
	client: reqwest::Client,

	/// Set to true when `promote()` is called; worker enters standby mode.
	promoted: AtomicBool,

	/// Wakes any blocking select in the streaming loop immediately on
	/// promotion.
	promote_notify: Notify,

	/// Runtime-overridden primary URL set by `demote()`. Takes precedence over
	/// `config.rocksdb_primary_url` when set.
	dynamic_primary_url: RwLock<Option<Url>>,

	/// Wakes the standby loop immediately when `demote()` is called.
	demote_notify: Notify,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &Args<'_>) -> Result<Arc<Self>> {
		let client = reqwest::Client::builder()
			.connect_timeout(Duration::from_secs(10))
			.build()
			.map_err(|e| err!(Database("Failed to build cluster HTTP client: {e}")))?;

		Ok(Arc::new(Self {
			db: args.db.clone(),
			server: args.server.clone(),
			services: args.services.clone(),
			global: args.db["global"].clone(),
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
	#[tracing::instrument(level = "trace", skip(self), err)]
	async fn worker(self: Arc<Self>) -> Result {
		if self.db.is_secondary() {
			// RocksDB opened in native secondary mode (read-only) -- WAL streaming
			// replication requires a writable database. Operator should use either
			// rocksdb_secondary OR rocksdb_primary_url, not both.
			warn!(
				"rocksdb_primary_url is set but database is in RocksDB native secondary mode \
				 (read-only); WAL streaming replication requires a writable database. Cluster \
				 worker will not run."
			);

			return Ok(());
		}

		while self.server.is_running() {
			// Resolve effective primary URL: dynamic (set by demote) takes
			// precedence over the static config value.
			let primary_url = self
				.dynamic_primary_url
				.read()
				.await
				.clone()
				.or_else(|| self.server.config.rocksdb_primary_url.clone());

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
			if self.is_promoted() {
				info!("In standalone primary mode; waiting for demote or shutdown.");
				tokio::select! {
					() = self.server.until_shutdown() => return Ok(()),
					() = self.demote_notify.notified() => {
						info!("Demote received; resuming replication from {primary_url}");
						continue;
					},
				}
			}

			// Bootstrap if no cursor is saved (first run or after WAL gap reset).
			let resume_seq = self.get_resume_seq()?;
			if resume_seq == 0 {
				self.bootstrap_resume_seq()?;
			}

			let mut backoff_ms = self
				.services
				.config
				.rocksdb_replication_backoff_min_ms;

			info!("Cluster worker starting; primary = {primary_url}");
			while self.server.is_running() && !self.is_promoted() {
				match self.sync(&primary_url).await {
					| Ok(()) => {
						// run_stream returns Ok on clean shutdown or promotion.
						if self.is_promoted() {
							// fall through to standby at top of outer loop
							debug!("Secondary cluster worker promoted");
							break;
						}

						// server is stopping
						debug!("Secondary cluster worker leaving...");
						return Ok(());
					},
					| Err(ref e) if is_wal_gap_error(e) => {
						warn!(
							"WAL gap detected — resume_seq is too old for new primary. \
							 Resetting cursor and restarting for clean checkpoint bootstrap."
						);

						if let Err(reset_err) = self.set_resume_seq(0) {
							return Err!(Database(error!(
								"WAL gap; stopping worker; failed to reset cursor for \
								 bootstrap: {reset_err}"
							)));
						}

						// Shut down cleanly — systemd will restart tuwunel via
						// Restart=on-failure.
						if let Err(e) = self.server.shutdown() {
							return Err!(Database(error!(
								"WAL gap; failed to trigger restart: {e}"
							)));
						}

						debug!("Secondary cluster worker with WAL gap leaving...");
						return Ok(());
					},
					| Err(ref e) => {
						if self.is_promoted() {
							debug!(?e, "Secondary cluster worker promoted");
							break;
						}

						error!("Replication sync error: {e}; reconnecting in {backoff_ms}ms...");
					},
				}

				// Exponential backoff with cap — also wakes on promotion or shutdown.
				tokio::select! {
					() = self.server.until_shutdown() => return Ok(()),
					() = self.promote_notify.notified() => break,
					() = sleep(Duration::from_millis(backoff_ms)) => {},
				}

				backoff_ms = backoff_ms.saturating_mul(2).min(
					self.services
						.config
						.rocksdb_replication_backoff_max_ms,
				);
			}
		}

		Ok(())
	}

	fn name(&self) -> &str { make_name(std::module_path!()) }
}

/// Demote this promoted primary back to a secondary replicating from
/// `new_primary_url`.
///
/// Does NOT reset the resume cursor — the worker will attempt WAL resume
/// from the new primary first. If the new primary returns 410 (WAL gap),
/// the worker resets to 0 and bootstraps automatically. This avoids a full
/// snapshot in the common case where the node was only down briefly.
///
/// The caller is responsible for ensuring the VIP / load balancer has been
/// updated to route writes to the new primary before calling this.
///
/// Returns `Err` if the instance is not currently promoted.
#[implement(Service)]
#[tracing::instrument(level = "info", skip(self), err)]
pub async fn demote(&self, new_primary_url: Url) -> Result {
	if !self.is_promoted() {
		return Err!(Database("This instance is not currently promoted; cannot demote."));
	}

	// Reset cursor so the worker bootstraps a fresh checkpoint from the new
	// primary rather than trying to resume from our own WAL position.
	self.set_resume_seq(0)?;

	// Store the new primary URL and clear the promoted flag before notifying
	// the worker so it sees a consistent state on wake-up.
	*self.dynamic_primary_url.write().await = Some(new_primary_url.clone());
	self.promoted.store(false, Ordering::Release);
	self.demote_notify.notify_waiters();

	info!("Demotion requested; will replicate from {new_primary_url}");
	Ok(())
}

/// Promote this secondary to a standalone primary.
///
/// Stops the cluster worker immediately. The caller is responsible for
/// updating the VIP / load balancer to route client traffic to this node.
#[implement(Service)]
#[tracing::instrument(level = "info", skip_all)]
pub fn promote(&self) {
	self.promoted.store(true, Ordering::Release);
	self.promote_notify.notify_waiters();
	info!("Promotion requested; stopping cluster worker.");
}

/// Returns true if this instance has been promoted to primary.
#[implement(Service)]
#[inline]
pub fn is_promoted(&self) -> bool { self.promoted.load(Ordering::Acquire) }

/// Read the secondary's persisted WAL resume cursor from the `global` column
/// family.
///
/// Returns `Ok(0)` when no cursor has been written yet (fresh secondary).
#[implement(Service)]
#[tracing::instrument(name = "get_resume_seq", level = "debug", skip_all, ret)]
fn get_resume_seq(&self) -> Result<u64> {
	use tuwunel_core::utils::result::NotFound;

	let result = self.global.get_blocking(b"primary_resume_seq");
	if result.is_not_found() {
		return Ok(0);
	}

	let handle: &[u8] = &result?;
	Ok(u64::from_le_bytes(handle.try_into()?))
}

/// Persist the secondary's WAL resume cursor to the `global` column family so
/// it survives restarts.
#[implement(Service)]
#[tracing::instrument(name = "set_resume_seq", level = "debug", skip(self), err)]
fn set_resume_seq(&self, seq: u64) -> Result {
	self.global
		.insert(b"primary_resume_seq", seq.to_le_bytes());

	Ok(())
}
