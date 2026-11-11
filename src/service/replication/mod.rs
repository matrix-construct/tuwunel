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
//!   -> load resume_seq (and the primary it belongs to) from replication_meta
//!   -> if there is no cursor for THIS primary: bootstrap (GET /checkpoint,
//!      restore, set resume_seq) — including on an ex-primary rejoining
//!   -> connect to GET /wal?since=<resume_seq>
//!   -> stream: for each frame apply batch, advance resume_seq, persist cursor
//!   -> on disconnect / error: exponential backoff, reconnect
//!   -> on 410 Gone (WAL gap): stop with error (manual restore required)
//!   -> on promote(): jump the PDU counter clear of the old primary's range,
//!      enter standby loop, instance becomes standalone primary
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
use futures::StreamExt;
use tokio::sync::{Notify, RwLock};
use tuwunel_core::{Result, err, error, info, warn};
use tuwunel_database::{Database, WalFrame, is_wal_gap_error};

use crate::service::{Args, make_name};

/// Marks the error `run_stream` returns when this replica should take its
/// one-time full checkpoint (`rocksdb_replication_auto_resync`). Worded to
/// match neither `is_wal_gap_error` nor the "WAL gap" the HA watcher script
/// greps for: this is a planned re-sync, handled here, not a gap.
const RESYNC_REQUIRED: &str = "one-time re-sync required";

/// Minimum retry delay after a transient connection error.
const BACKOFF_MIN_MS: u64 = 500;
/// Maximum retry delay (caps the exponential backoff).
const BACKOFF_MAX_MS: u64 = 30_000;

pub struct Service {
	db: Arc<Database>,
	server: Arc<tuwunel_core::Server>,
	services: Arc<crate::services::OnceServices>,
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
	/// Held while a frame is applied and for the whole of `promote()`, so a
	/// promotion sees the cursor exactly as the last applied frame left it and
	/// no frame is applied after the handoff has been recorded.
	apply_lock: std::sync::Mutex<()>,
	/// Set once this process has been a primary (promoted, or demoted from
	/// one). Its writes since startup are then a primary's writes, which the
	/// clean-stop proof from startup says nothing about: no fast rejoin.
	was_primary: AtomicBool,
	/// Fast rejoin is attempted at most once per process.
	rejoin_tried: AtomicBool,
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
			services: args.services.clone(),
			client,
			promoted: AtomicBool::new(false),
			promote_notify: Notify::new(),
			dynamic_primary_url: RwLock::new(None),
			demote_notify: Notify::new(),
			apply_lock: std::sync::Mutex::new(()),
			was_primary: AtomicBool::new(false),
			rejoin_tried: AtomicBool::new(false),
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
			if !self.server.is_running() {
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

			let db_path = self.server.config.database_path.clone();
			let parent = db_path.parent().unwrap_or(&db_path);
			let sidecar = parent.join("_replication_needs_bootstrap");

			// Sidecar wins when present: the just-completed bootstrap wrote
			// it with the checkpoint's actual seq. The checkpoint may carry
			// a stale `resume_seq` in `replication_meta` from when the
			// primary was previously a secondary; honoring it would loop us
			// back into a gap on every bootstrap.
			if sidecar.exists() {
				let seq_str = std::fs::read_to_string(&sidecar)
					.map_err(|e| err!(Database("Reading bootstrap sidecar: {e}")))?;
				// "<seq>" or "<seq> <primary instance id>".
				let mut fields = seq_str.split_whitespace();
				let seq: u64 = fields.next().unwrap_or_default().parse()
					.map_err(|e| err!(Database("Parsing bootstrap sidecar: {e}")))?;
				let checkpoint_instance = fields.next();
				self.db.set_replication_resume_seq(seq)?;
				self.db.set_replication_primary(Some(&primary_url))?;
				// The checkpoint carries the primary's own replication_meta,
				// including whatever instance *it* once replicated from.
				// Replace it with the database the checkpoint came from, so
				// the stream is checked against that from the very first
				// connection: a primary that reopened as a different database
				// in the meantime (`rejoin::open_identity`) is then a WAL gap
				// rather than a silent splice. `None` only for a primary on a
				// build that does not identify itself.
				self.db.set_replication_primary_instance(checkpoint_instance)?;
				// The restore marked this already; again here so that a mark
				// lost to a write error cannot turn the one-time re-sync into
				// a repeating one.
				if let Some(instance) = checkpoint_instance
					&& let Err(e) = self.db.mark_exact_synced(instance)
				{
					warn!("Could not record that this database is synced: {e}");
				}
				self.db.set_promotion_pending(false);
				std::fs::remove_file(&sidecar)
					.map_err(|e| err!(Database("Removing bootstrap sidecar: {e}")))?;
				info!("Bootstrap complete via pre-open path; resume_seq = {seq}");
			} else {
				// No sidecar — steady-state re-open, or first start of an ex-primary.
				//
				// Resume only from a cursor recorded against THIS primary. A
				// RocksDB sequence number is local to the database that issued
				// it, so neither our own sequence (an ex-primary rejoining) nor
				// a cursor recorded against a previous primary names any point
				// in this primary's WAL. Streaming from one either replays
				// batches we already hold or — silently — skips batches we never
				// got. A checkpoint restore is ~2 minutes; bootstrap instead.
				let resume_seq = self.db.get_replication_resume_seq()?;
				let cursor_primary = self.db.get_replication_primary()?;
				let cursor_is_ours = cursor_primary.as_deref() == Some(primary_url.as_str());
				if resume_seq == 0 || !cursor_is_ours {
					// An ex-primary that can prove it lost nothing resumes from
					// the new primary's WAL instead of copying the database.
					if resume_seq == 0
						&& cursor_primary.is_none()
						&& self.try_fast_rejoin(&primary_url).await
					{
						continue;
					}

					// The attempt can wait a long time on an unreachable
					// primary — long enough for this node to be promoted or
					// told to stop. Either way it must not go on to discard
					// its database: back to the top, which handles both.
					if self.promoted.load(Ordering::Acquire) || !self.server.is_running() {
						continue;
					}

					let reason = if resume_seq == 0 && self.db.latest_wal_sequence() == 0 {
						"Empty database".to_owned()
					} else if resume_seq == 0 {
						"No resume cursor (this node was primary)".to_owned()
					} else {
						format!(
							"Resume cursor belongs to {}, not {primary_url}",
							cursor_primary.as_deref().unwrap_or("an unrecorded primary")
						)
					};
					// Under the apply lock, with the promotion check repeated:
					// `promote()` holds the same lock, so this node cannot be
					// promoted between the check and the restart that would
					// throw its database away.
					let _guard = self.apply_lock.lock().expect("apply lock poisoned");
					if self.promoted.load(Ordering::Acquire) {
						continue;
					}
					std::fs::write(&sidecar, "0")
						.map_err(|e| err!(Database("Writing bootstrap trigger: {e}")))?;
					info!("{reason}; bootstrap required. Triggering restart.");
					return self.restart_for_bootstrap();
				}
			}

			info!("Replication worker starting; primary = {primary_url}");

			let mut backoff_ms = BACKOFF_MIN_MS;

			while self.server.is_running() && !self.promoted.load(Ordering::Acquire) {
				match self.run_stream(&primary_url).await {
					| Ok(()) => {
						// run_stream returns Ok on clean shutdown or promotion.
						if self.promoted.load(Ordering::Acquire) {
							break; // fall through to standby at top of outer loop
						}
						return Ok(()); // server is stopping
					},
					| Err(ref e) if e.to_string().contains(RESYNC_REQUIRED) => {
						// Not if this node was promoted meanwhile.
						let _guard = self.apply_lock.lock().expect("apply lock poisoned");
						if self.promoted.load(Ordering::Acquire) {
							break;
						}
						warn!(
							"This replica's data predates exact replication and is missing \
							 writes. Restoring one full checkpoint from the primary to repair \
							 it; this happens once. (Set rocksdb_replication_auto_resync = false \
							 to do it by hand instead.)"
						);
						let sidecar = self.server.config.database_path
							.parent()
							.unwrap_or(&self.server.config.database_path)
							.join("_replication_needs_bootstrap");
						std::fs::write(&sidecar, "0")
							.map_err(|e| err!(Database("Writing bootstrap trigger: {e}")))?;
						self.db.set_replication_resume_seq(0)?;
						return self.restart_for_bootstrap();
					},
					| Err(ref e) if is_wal_gap_error(e) => {
						warn!(
							"WAL gap detected — resume_seq is too old for primary. \
							 Writing bootstrap sidecar and restarting for clean checkpoint restore."
						);
						// As above: not if this node was promoted meanwhile.
						let _guard = self.apply_lock.lock().expect("apply lock poisoned");
						if self.promoted.load(Ordering::Acquire) {
							break;
						}
						let sidecar = self.server.config.database_path
							.parent()
							.unwrap_or(&self.server.config.database_path)
							.join("_replication_needs_bootstrap");
						if let Err(e) = std::fs::write(&sidecar, "0") {
							error!("Failed to write bootstrap sidecar after WAL gap: {e}");
						}
						if let Err(e) = self.db.set_replication_resume_seq(0) {
							error!("Failed to reset resume_seq after WAL gap: {e}");
						}
						return self.restart_for_bootstrap();
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
	/// Restart the process so the pre-open path can restore a checkpoint; the
	/// sidecar must already be written.
	fn restart_for_bootstrap(&self) -> Result {
		if let Err(e) = self.server.shutdown() {
			error!("Failed to trigger shutdown for checkpoint bootstrap: {e}");
			return Err(err!(Database("Bootstrap required; failed to trigger restart")));
		}
		Ok(())
	}

	/// Promote this secondary to a standalone primary.
	///
	/// Stops the replication worker immediately and clears `resume_seq` so a
	/// future demote (or checkpoint served to a future secondary) doesn't
	/// carry stale secondary-era state. The caller is responsible for
	/// updating the VIP / load balancer to route client traffic to this node.
	///
	/// Also jumps the PDU counter past anything the old primary could have
	/// issued (see `PROMOTION_COUNTER_GAP`). That has to happen here rather
	/// than on the restart that health-check.sh performs afterwards: this
	/// process accepts writes as primary the moment the flag below is set —
	/// the appservice is repointed at it in the same step — and until now its
	/// in-memory counter was frozen at wherever this replica started. If the
	/// jump fails, promotion fails; a primary numbering events inside the old
	/// primary's range is worse than no failover.
	pub fn promote(&self) -> Result<()> {
		// No frame is applied from here until `promoted` is set, after which
		// `apply_frame` refuses them: the cursor read below is final.
		let _guard = self.apply_lock.lock().expect("apply lock poisoned");

		// What this node knows about the primary it is replacing, read before
		// the cursor is cleared. See `tuwunel_database::rejoin`.
		let applied_seq = self.db.get_replication_resume_seq()?;
		let old_primary = self.db.get_replication_primary()?;
		let old_instance = self.db.get_replication_primary_instance()?;

		// From here until the counter has jumped, the cursor is gone but the
		// jump has not happened. Mark that, first: if this process dies in
		// between and comes back as a primary, startup sees the mark and
		// applies the jump (`globals::Data::new`). WAL order guarantees that
		// if the clears below survive a crash, so does the mark.
		self.db.set_promotion_pending(true);

		// Clear the cursor so a future demote (or a checkpoint served to a
		// future secondary) carries no secondary-era state. Deliberately
		// *before* `resume_after` is read: these are this node's own
		// bookkeeping, and a rejoining ex-primary that streamed them would
		// have its own cursor wiped by them.
		self.db.set_replication_resume_seq(0)?;
		self.db.set_replication_primary(None)?;
		self.db.set_replication_primary_instance(None)?;

		// Everything this node writes from here on has a later sequence, and
		// is therefore in the WAL an ex-primary resumes from. Purity is read
		// *after* this: a local data write that slipped in earlier dropped the
		// marker before it reached RocksDB, so it cannot be missed here.
		let resume_after = self.db.latest_wal_sequence();
		let handoff = match (old_instance.clone(), self.db.pure_replica_of()) {
			| (Some(from_instance), Some(pure_of))
				if from_instance == pure_of && applied_seq > 0 =>
				Some(tuwunel_database::rejoin::Handoff {
					from_instance,
					from_applied_seq: applied_seq,
					resume_after,
					instance: self.db.instance_id().to_owned(),
				}),
			| _ => None,
		};

		// The handoff is a shortcut for the node being replaced, not part of
		// the failover: if it cannot be written, promote without one and that
		// node restores a checkpoint, as it always used to.
		let handoff = match handoff {
			| Some(handoff) => match self.db.write_handoff(&handoff) {
				| Ok(()) => Some(handoff),
				| Err(e) => {
					warn!("Could not record a handoff; the replaced primary will need a full checkpoint: {e}");
					self.db.clear_handoff();
					None
				},
			},
			| None => {
				self.db.clear_handoff();
				None
			},
		};

		// Jump the counter after `resume_after`, so it reaches a rejoining
		// ex-primary through the stream like any other write.
		let count = match self
			.services
			.globals
			.advance_counter_for_promotion()
		{
			| Ok(count) => count,
			| Err(e) => {
				// Not promoted. Put the cursor back so this node carries on
				// as the replica it still is, and leave no handoff behind.
				self.db.clear_handoff();
				self.db
					.set_replication_primary_instance(old_instance.as_deref())?;
				self.db.set_replication_primary(old_primary.as_deref())?;
				self.db.set_replication_resume_seq(applied_seq)?;
				self.db.set_promotion_pending(false);
				return Err(e);
			},
		};
		self.db.set_promotion_pending(false);

		self.was_primary.store(true, Ordering::Release);
		self.promoted.store(true, Ordering::Release);
		self.promote_notify.notify_waiters();
		match &handoff {
			| Some(h) => info!(
				count,
				from_instance = %h.from_instance,
				from_applied_seq = h.from_applied_seq,
				resume_after = h.resume_after,
				"Promotion requested; counter advanced clear of the previous primary; \
				 handoff recorded for its fast rejoin; stopping replication worker."
			),
			| None => info!(
				count,
				"Promotion requested; counter advanced clear of the previous primary; no \
				 handoff recorded (this node is not a proven exact copy of it, so it will \
				 need a full checkpoint to rejoin); stopping replication worker."
			),
		}
		Ok(())
	}

	/// Returns true if this instance has been promoted to primary.
	pub fn is_promoted(&self) -> bool { self.promoted.load(Ordering::Acquire) }

	/// Demote this promoted primary back to a secondary replicating from
	/// `new_primary_url`.
	///
	/// The worker then bootstraps from a checkpoint of the new primary: this
	/// node's cursor was cleared on promotion, and its own sequence numbers
	/// mean nothing against another database's WAL, so there is no safe point
	/// to resume from.
	///
	/// The caller is responsible for ensuring the VIP / load balancer has been
	/// updated to route writes to the new primary before calling this.
	///
	/// Returns `Err` if the instance is not currently promoted.
	pub async fn demote(&self, new_primary_url: String) -> Result<()> {
		if !self.promoted.load(Ordering::Acquire) {
			return Err(err!(Database(
				"This instance is not currently promoted; cannot demote."
			)));
		}

		// This process is about to apply another primary's WAL as raw frames,
		// which never reach the /sync change index's hooks: retire it for good.
		self.services.sync.retire_change_index();

		// The handoff described this node as the primary; it no longer is.
		self.db.clear_handoff();
		self.was_primary.store(true, Ordering::Release);

		// Store the new primary URL and clear the promoted flag before notifying
		// the worker so it sees a consistent state on wake-up.
		*self.dynamic_primary_url.write().await = Some(new_primary_url.clone());
		self.promoted.store(false, Ordering::Release);
		self.demote_notify.notify_waiters();

		info!("Demotion requested; will bootstrap from {new_primary_url}");
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

		// A primary that names its database also sends an exact stream (see
		// `StreamState`); one that does not is an older build.
		let primary_instance = resp
			.headers()
			.get("x-tuwunel-instance-id")
			.and_then(|v| v.to_str().ok())
			.map(ToOwned::to_owned);
		self.note_primary_instance(primary_instance.as_deref())?;

		// A database that has never been synced under exact replication ran a
		// build whose stream dropped batches, and is missing them still. Only
		// worth repairing from a primary that identifies itself — one that
		// does not is on such a build too, and would drop them again.
		if primary_instance.is_some()
			&& self.server.config.rocksdb_replication_auto_resync
			&& !self.db.exact_synced()
		{
			return Err(err!(Database("{RESYNC_REQUIRED}")));
		}

		info!("WAL stream connected; starting from seq {resume_seq}");

		let mut byte_stream = resp.bytes_stream();
		let mut buf: Vec<u8> = Vec::new();
		let mut state = StreamState {
			cursor: resume_seq,
			exact: primary_instance.is_some(),
		};

		while self.server.is_running() && !self.promoted.load(Ordering::Acquire) {
			tokio::select! {
				chunk = byte_stream.next() => {
					let Some(chunk) = chunk else {
						return Err(err!(Database("Primary closed WAL stream")));
					};
					let chunk = chunk.map_err(|e| err!(Database("WAL stream read: {e}")))?;
					buf.extend_from_slice(&chunk);
					self.drain_frames(&mut buf, &mut state)?;
				},
				() = self.server.until_shutdown() => return Ok(()),
				() = self.promote_notify.notified() => return Ok(()),
			}
		}

		Ok(())
	}

	/// Parse and apply as many complete frames as possible from `buf`.
	fn drain_frames(&self, buf: &mut Vec<u8>, state: &mut StreamState) -> Result {
		let mut offset = 0;
		while let Ok((frame, consumed)) = WalFrame::decode(&buf[offset..]) {
			self.apply_frame(&frame, state)?;
			#[allow(clippy::arithmetic_side_effects)]
			{
				offset += consumed;
			}
		}
		buf.drain(..offset);
		Ok(())
	}

	/// Apply a single frame to the local database.
	fn apply_frame(&self, frame: &WalFrame, state: &mut StreamState) -> Result {
		use tuwunel_database::FRAME_TYPE_DATA;

		// Batch and cursor move together, and not at all once promoted:
		// `promote()` holds this lock while it records how far we got.
		let _guard = self.apply_lock.lock().expect("apply lock poisoned");
		if self.promoted.load(Ordering::Acquire) {
			return Ok(());
		}

		let is_data = frame.frame_type == FRAME_TYPE_DATA;

		// From a primary that sends an exact stream, every data frame is the
		// batch directly after the cursor and every heartbeat names the cursor
		// itself. Anything else means batches are missing between the two, and
		// applying past a hole would leave it there for good: report it as a
		// WAL gap, which re-syncs from a checkpoint.
		if state.exact && state.cursor != 0 {
			let expected = if is_data {
				state.cursor.saturating_add(1)
			} else {
				state.cursor
			};
			if frame.sequence != expected {
				return Err(err!(Database(
					"WAL gap: stream out of sequence — cursor is {}, {} frame is at {}",
					state.cursor,
					if is_data { "data" } else { "heartbeat" },
					frame.sequence
				)));
			}
		}

		if is_data && !frame.batch_data.is_empty() {
			self.db.write_raw_batch(&frame.batch_data)?;
		}

		let next = frame.resume_seq();
		if next > 0 && next != state.cursor {
			self.db.set_replication_resume_seq(next)?;
			state.cursor = next;
		}
		Ok(())
	}

	/// Record which database the stream belongs to.
	///
	/// A cursor is a position in one particular database's WAL. If the primary
	/// now reports a *different* database than the one the cursor was recorded
	/// against — it was wiped, restored, or lost its unsynced tail in a crash
	/// (`rejoin::open_identity`) — the cursor names nothing in it, and
	/// resuming would splice two histories together. That is a WAL gap:
	/// re-sync from a checkpoint.
	fn note_primary_instance(&self, instance: Option<&str>) -> Result {
		let recorded = self.db.get_replication_primary_instance()?;

		if let (Some(recorded), Some(instance)) = (recorded.as_deref(), instance)
			&& recorded != instance
		{
			return Err(err!(Database(
				"WAL gap: the primary's database is {instance}, but this node's cursor \
				 belongs to {recorded}"
			)));
		}

		// Either side unidentified: a primary on an older build, or the first
		// connection since this node learned to record it. Carry on as that
		// build would have, but an unidentified primary vouches for nothing.
		if recorded.as_deref() != instance {
			self.db.set_replication_primary_instance(instance)?;
		}
		if let Some(pure_of) = self.db.pure_replica_of()
			&& Some(pure_of.as_str()) != instance
		{
			self.db.revoke_pure_replica();
		}
		Ok(())
	}

	/// Try to resume from the new primary's WAL instead of restoring a
	/// checkpoint. Returns true only when the cursor has been set from a
	/// handoff that proves this database lost nothing; see
	/// `tuwunel_database::rejoin` for the proof. Every other outcome —
	/// including "could not tell" — is false, and the caller bootstraps.
	async fn try_fast_rejoin(&self, primary_url: &str) -> bool {
		use tuwunel_database::rejoin::TailCheck;

		if self.rejoin_tried.swap(true, Ordering::AcqRel) {
			return false;
		}

		// The clean-stop proof covers the database as it was when this
		// process opened it. It says nothing about writes this process made as
		// a primary, so the process must have been a replica throughout.
		if self.was_primary.load(Ordering::Acquire)
			|| self.promoted.load(Ordering::Acquire)
			|| self.server.config.rocksdb_primary_url.is_none()
		{
			return false;
		}

		let Some(stop) = self.db.proven_clean_stop() else {
			info!(
				"Fast rejoin: not possible — the previous run did not end in a recorded \
				 clean stop."
			);
			return false;
		};

		let Some(handoff) = self.fetch_handoff(primary_url).await else {
			return false;
		};

		let mine = self.db.instance_id();
		if handoff.from_instance != mine {
			info!(
				"Fast rejoin: not possible — the primary's handoff is for database {}, this \
				 one is {mine}.",
				handoff.from_instance
			);
			return false;
		}
		if handoff.from_applied_seq > stop.last_seq {
			warn!(
				"Fast rejoin: refused — the primary claims to have applied seq {} of this \
				 database, which only reached {}.",
				handoff.from_applied_seq, stop.last_seq
			);
			return false;
		}

		// The writes the new primary never received: (from_applied_seq, last_seq].
		let db = self.db.clone();
		let (after, through) = (handoff.from_applied_seq, stop.last_seq);
		let tail = tokio::task::spawn_blocking(move || db.check_wal_tail(after, through)).await;
		let writes = match tail {
			| Ok(TailCheck::NodeLocalOnly { writes }) => writes,
			| Ok(TailCheck::DataWrite { column, key }) => {
				info!(
					"Fast rejoin: not possible — this database made a write the new primary \
					 never received (column {column}, key {key}, after seq {after})."
				);
				return false;
			},
			| Ok(TailCheck::Unreadable(why)) => {
				info!(
					"Fast rejoin: not possible — could not read this database's own WAL for \
					 seq {after}..={through}: {why}"
				);
				return false;
			},
			| Err(e) => {
				warn!("Fast rejoin: tail check task failed: {e}");
				return false;
			},
		};

		// The proof so far is about the database as it stood at the clean
		// stop. This process has been running since then — possibly for a
		// long time, waiting for the primary — and a replica is a full
		// homeserver: anything that reached it could have written. Declaring
		// this database an exact copy of the new primary is refused if this
		// process has made any local data write, and that refusal is the last
		// part of the proof. Without it, such a write would be kept where a
		// checkpoint restore would have discarded it.
		//
		// Under the apply lock, so none of this can interleave with a
		// promotion of this node.
		let _guard = self.apply_lock.lock().expect("apply lock poisoned");
		if self.promoted.load(Ordering::Acquire) {
			return false;
		}

		match self.db.assert_pure_replica_of(&handoff.instance) {
			| Ok(true) => {},
			| Ok(false) => {
				info!(
					"Fast rejoin: not possible — this node has written data of its own \
					 since it started."
				);
				return false;
			},
			| Err(e) => {
				warn!("Fast rejoin: could not record the exact-copy marker: {e}");
				return false;
			},
		}

		// Cursor last: until it is non-zero this is still "an ex-primary with
		// no cursor", and a crash part-way through bootstraps.
		let set = self
			.db
			.set_replication_primary_instance(Some(&handoff.instance))
			.and_then(|()| self.db.set_replication_primary(Some(primary_url)))
			.and_then(|()| self.db.set_replication_resume_seq(handoff.resume_after));

		// A data write that landed between the marker and the cursor dropped
		// the marker again. Take the cursor back: no proof, no shortcut.
		let still_exact =
			self.db.pure_replica_of().as_deref() == Some(handoff.instance.as_str());
		if set.is_err() || !still_exact {
			let _ = self.db.set_replication_resume_seq(0);
			let _ = self.db.set_replication_primary(None);
			let _ = self.db.set_replication_primary_instance(None);
			self.db.revoke_pure_replica();
			match set {
				| Err(e) => warn!("Fast rejoin: could not record the cursor: {e}"),
				| Ok(()) => info!(
					"Fast rejoin: not possible — this node wrote data of its own while \
					 rejoining."
				),
			}
			return false;
		}
		self.db.clear_handoff();
		// Proven equal to the new primary: as good as a fresh checkpoint.
		if let Err(e) = self.db.mark_exact_synced(&handoff.instance) {
			warn!("Could not record that this database is synced: {e}");
		}

		info!(
			resume_after = handoff.resume_after,
			from_applied_seq = handoff.from_applied_seq,
			last_seq = stop.last_seq,
			bookkeeping_writes = writes,
			"Fast rejoin: this database lost nothing. Resuming from the new primary's WAL \
			 instead of restoring a checkpoint."
		);
		true
	}

	/// Fetch the primary's handoff record. Waits out a primary that is not
	/// answering — giving up would spend the clean-stop proof on a restart —
	/// but returns `None` as soon as the primary answers anything other than a
	/// record.
	async fn fetch_handoff(
		&self,
		primary_url: &str,
	) -> Option<tuwunel_database::rejoin::Handoff> {
		#[derive(serde::Deserialize)]
		struct Body {
			from_instance: String,
			from_applied_seq: u64,
			resume_after: u64,
			instance: String,
		}

		let url = format!("{primary_url}/_tuwunel/replication/handoff");
		let mut backoff_ms = BACKOFF_MIN_MS;
		loop {
			if !self.server.is_running() || self.promoted.load(Ordering::Acquire) {
				return None;
			}

			let attempt = tokio::time::timeout(Duration::from_secs(15), async {
				let resp = self.authed_get(&url).await?;
				let status = resp.status();
				let body = resp.text().await?;
				Ok::<_, reqwest::Error>((status, body))
			})
			.await;

			match attempt {
				| Ok(Ok((status, body))) if status.is_success() => {
					return match serde_json::from_str::<Body>(&body) {
						| Ok(b) => Some(tuwunel_database::rejoin::Handoff {
							from_instance: b.from_instance,
							from_applied_seq: b.from_applied_seq,
							resume_after: b.resume_after,
							instance: b.instance,
						}),
						| Err(e) => {
							warn!("Fast rejoin: unreadable handoff from {primary_url}: {e}");
							None
						},
					};
				},
				// The primary is up and has no handoff for us (or cannot give
				// one: an older build, a refused token). A definite answer.
				| Ok(Ok((status, _))) if status.is_client_error() => {
					info!(
						"Fast rejoin: not possible — the primary has no handoff record \
						 ({status})."
					);
					return None;
				},
				| Ok(Ok((status, _))) => {
					info!("Fast rejoin: primary answered {status} for the handoff; retrying.");
				},
				| Ok(Err(e)) => {
					info!("Fast rejoin: primary not reachable for the handoff ({e}); retrying.");
				},
				| Err(_) => {
					info!("Fast rejoin: primary timed out on the handoff; retrying.");
				},
			}

			tokio::select! {
				() = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {},
				() = self.server.until_shutdown() => return None,
				() = self.promote_notify.notified() => return None,
			}
			#[allow(clippy::arithmetic_side_effects)]
			{
				backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
			}
		}
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

/// Per-connection view of the replication cursor.
struct StreamState {
	/// Last sequence of the primary's WAL this database holds.
	cursor: u64,
	/// The primary identified its database, which only builds with an exact
	/// stream do: frames are then checked against the cursor. An older
	/// primary's stream drops and overstates batches as a matter of course,
	/// and holding it to the same check would re-sync from it endlessly.
	exact: bool,
}
