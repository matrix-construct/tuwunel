use std::{ops::Range, sync::Arc};

use futures::TryFutureExt;
use tokio::sync::watch::Sender;
use tuwunel_core::{
	Result, err, info, warn,
	matrix::{PduCount, RawPduId},
	utils,
	utils::two_phase_counter::{Counter as TwoPhaseCounter, Permit as TwoPhasePermit},
};
use tuwunel_database::{Database, Deserialized, Map};

pub struct Data {
	global: Arc<Map>,
	retires: Sender<u64>,
	counter: Arc<Counter>,
	pub(super) db: Arc<Database>,
}

pub(super) type Permit = TwoPhasePermit<Callback>;
type Counter = TwoPhaseCounter<Callback>;
type Callback = Box<dyn Fn(u64) -> Result + Send + Sync>;

const COUNTER: &[u8] = b"c";

/// How far a promotion moves the counter past everything this node knows of.
///
/// Replication is asynchronous, so when a primary dies it has usually issued
/// counts this replica never received, and clients that synced against it hold
/// `since` tokens inside that unreplicated tail. A client is only ever sent
/// events numbered above its token, so a new primary that resumed numbering
/// inside the tail would hide every new event there from every such client —
/// silently, and permanently for those events. No replica can know the exact
/// extent of a tail it never received, so the new primary starts clear of any
/// plausible one instead.
pub(super) const PROMOTION_COUNTER_GAP: u64 = 100_000_000;

/// High-water mark written by a graceful shutdown. Its presence means the
/// counter was persisted in full and `pduid_pdu` need not be scanned; its
/// absence means the process stopped without recording one and the scan is
/// required. Cleared as soon as it is consumed at startup.
const CLEAN_COUNTER: &[u8] = b"clean_c";

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Result<Self> {
		let db = args.db.clone();
		let global = args.db["global"].clone();

		// `global['c']` is cork-buffered and can lag behind durable `pduid_pdu`
		// writes; on promote-restart, derive the high-water mark from both so
		// the new primary doesn't re-issue colliding counts.
		let from_global_c = Self::stored_count(&global)?;

		// Recovering that mark from `pduid_pdu` reads every PDU key in the
		// database. That is ~60 minutes on a 12GB/868M-event database, and the
		// listener is not bound for any of it, so peers' health probes see this
		// node as down and a restarting cluster can elect two primaries.
		//
		// A graceful shutdown records the mark under CLEAN_COUNTER once nothing
		// can still dispatch a count, so there is nothing left to recover and
		// the scan is skipped. It is only needed after an unclean stop, which
		// is the one case where `global['c']` may genuinely lag durable
		// `pduid_pdu` writes.
		//
		// A replica needs no scan at all. It issues no counts while it is a
		// replica — every PDU it holds arrives already numbered, through the
		// WAL stream — and promotion does not rely on this value either:
		// `replication::Service::promote` moves the counter clear of the old
		// primary's range itself, before the node takes a write. So a replica
		// skips the scan and starts from the stored counter.
		let is_replica = args.server.config.rocksdb_primary_url.is_some();
		let from_clean = Self::stored_clean_count(&global);
		let from_pdus = match from_clean {
			| Some(clean) => {
				info!(clean, "Clean shutdown recorded: skipping PDU high-water scan.");
				clean
			},
			| None if is_replica => {
				info!(
					"Starting as a replica: skipping PDU high-water scan. A replica issues no \
					 counts, and promotion advances the counter itself."
				);
				0
			},
			| None => {
				warn!(
					"No clean shutdown was recorded. Recovering the PDU high-water mark by \
					 scanning pduid_pdu; this is proportional to database size and the \
					 listener stays unbound until it completes."
				);
				Self::max_pdu_count_across_rooms(&args.db["pduid_pdu"], args.server)?
			},
		};

		// Consume the marker so that an unclean stop from here on forces the
		// scan. Synced, because a marker that outlived the crash it was meant to
		// precede would be trusted on the next boot and the lag it exists to
		// catch would go unnoticed.
		if from_clean.is_some() {
			let _cork = db.cork_and_sync();
			global.remove(CLEAN_COUNTER);
		}

		info!(from_global_c, from_pdus, "Recovered PDU counter high-water mark");

		let mut count = from_global_c.max(from_pdus);

		// Becoming primary without `replication::Service::promote`.
		//
		// promote() is where a replica's counter normally jumps clear of the
		// old primary's range (see PROMOTION_COUNTER_GAP), and it clears the
		// replication cursor as it does. A database that starts as primary with
		// that cursor still in place was therefore a replica until now and
		// skipped the jump — started as primary by hand, or by a script path
		// that bypassed promote(). Its clients could otherwise hold sync tokens
		// above everything it is about to issue. Apply the jump here, persist
		// it, and clear the cursor so the next restart does not jump again.
		let was_replica = args.db.get_replication_primary().ok().flatten().is_some()
			|| args.db.get_replication_resume_seq().unwrap_or(0) > 0;
		if !is_replica && was_replica {
			count = count.saturating_add(PROMOTION_COUNTER_GAP);
			warn!(
				count,
				"Starting as primary on a database that was a replica and was never promoted; \
				 advancing the PDU counter clear of the previous primary's range."
			);
			let _cork = db.cork_and_sync();
			Self::store_count(&db, &global, count)?;
			args.db.set_replication_resume_seq(0)?;
			args.db.set_replication_primary(None)?;
		}
		let retires = Sender::new(count);
		Ok(Self {
			db: args.db.clone(),
			global: args.db["global"].clone(),
			retires: retires.clone(),
			counter: Counter::new(
				count,
				Box::new(move |count| Self::store_count(&db, &db["global"], count)),
				Box::new(move |count| Self::handle_retire(&retires, count)),
			),
		})
	}

	#[inline]
	pub(super) async fn wait_pending(&self) -> Result<u64> {
		let count = self.counter.dispatched();
		self.wait_count(&count).await.inspect(|retired| {
			debug_assert!(
				*retired >= count,
				"Expecting retired sequence number >= snapshotted dispatch number"
			);
		})
	}

	#[inline]
	pub(super) async fn wait_count(&self, count: &u64) -> Result<u64> {
		self.retires
			.subscribe()
			.wait_for(|retired| retired.ge(count))
			.map_ok(|retired| *retired)
			.map_err(|e| err!(debug_error!("counter channel error {e:?}")))
			.await
	}

	#[inline]
	pub(super) fn next_count(&self) -> Permit {
		self.counter
			.next()
			.expect("failed to obtain next sequence number")
	}

	#[inline]
	pub(super) fn current_count(&self) -> u64 { self.counter.current() }

	#[inline]
	pub(super) fn pending_count(&self) -> Range<u64> { self.counter.range() }

	#[tracing::instrument(name = "retire", level = "debug", skip(sender))]
	fn handle_retire(sender: &Sender<u64>, count: u64) -> Result {
		let _prev = sender.send_replace(count);

		Ok(())
	}

	#[tracing::instrument(name = "dispatch", level = "debug", skip(db, global))]
	fn store_count(db: &Arc<Database>, global: &Arc<Map>, count: u64) -> Result {
		let _cork = db.cork();
		global.insert(COUNTER, count.to_be_bytes());

		Ok(())
	}

	fn stored_count(global: &Arc<Map>) -> Result<u64> {
		global
			.get_blocking(COUNTER)
			.as_deref()
			.map_or(Ok(0_u64), utils::u64_from_bytes)
	}

	/// High-water mark recorded by a graceful shutdown, if there is one.
	///
	/// Any read or decode failure is reported as absent, which fails safe: the
	/// caller then recovers by scanning rather than trusting a value it could
	/// not read.
	fn stored_clean_count(global: &Arc<Map>) -> Option<u64> {
		global
			.get_blocking(CLEAN_COUNTER)
			.as_deref()
			.ok()
			.and_then(|bytes| utils::u64_from_bytes(bytes).ok())
	}

	/// Record the counter's high-water mark so the next startup can skip the
	/// `pduid_pdu` recovery scan.
	///
	/// Must only be called once nothing can dispatch a further count, or the
	/// recorded mark would be lower than what was actually issued. Synced
	/// before returning: a mark left in the write buffer would be lost by the
	/// very shutdown it is describing, and the next startup would fall back to
	/// scanning — correct, but slow for no reason.
	///
	/// On a replica the in-memory counter never moves — replicated writes land
	/// in `global['c']` without passing through it — so it only says where the
	/// counter stood when this process started. The stored counter is what the
	/// primary has been advancing all along, so record whichever is higher.
	/// On a primary the stored counter never leads the in-memory one, and this
	/// is unchanged.
	pub(super) fn persist_clean_shutdown(&self) -> u64 {
		let stored = Self::stored_count(&self.global).unwrap_or(0);
		let count = self.counter.dispatched().max(stored);
		let _cork = self.db.cork_and_sync();
		self.global.insert(CLEAN_COUNTER, count.to_be_bytes());

		count
	}

	/// Move the counter past anything the old primary could have issued, as
	/// this node becomes primary. See `PROMOTION_COUNTER_GAP`.
	///
	/// Starts from the higher of the in-memory counter and the stored one: on a
	/// replica the in-memory value is only where the counter stood when this
	/// process started, while `global['c']` has been following the primary
	/// through the WAL. Synced before returning, so the jump survives a crash
	/// in the window before the next write would have persisted it.
	pub(super) fn advance_for_promotion(&self, gap: u64) -> Result<u64> {
		let stored = Self::stored_count(&self.global)?;
		let base = self.counter.dispatched().max(stored);
		let target = base.saturating_add(gap);
		let _cork = self.db.cork_and_sync();
		self.counter.advance_to(target)
	}

	/// Largest `Normal` PDU count across `pduid_pdu`. Backfilled counts are
	/// ignored (not drawn from the global counter). Returns 0 if empty.
	///
	/// Reads every key in the column family, so this is O(total events) and
	/// takes ~60 minutes on a 12 GB / 868M-event database. It cannot be made
	/// cheap — every iterator here runs with `total_order_seek`, so seeking
	/// per-room costs more index reads than stepping does — which is why the
	/// caller avoids reaching it rather than optimising it.
	///
	/// Checks for shutdown every `SCAN_SHUTDOWN_CHECK_EVERY` keys and gives up
	/// with an error if one was requested. The scan runs before the signal
	/// handlers can act on anything, so without this a `systemctl stop` sat
	/// out the whole scan and was only ended by systemd's SIGKILL — on
	/// core2-phx on 2026-09-23 that took the full ~6 minute stop timeout. An
	/// abandoned scan leaves nothing half-done: no marker is written, and the
	/// next start simply scans again.
	fn max_pdu_count_across_rooms(pduid_pdu: &Arc<Map>, server: &tuwunel_core::Server) -> Result<u64> {
		const SCAN_SHUTDOWN_CHECK_EVERY: u64 = 1 << 20;
		let mut max: u64 = 0;
		let mut seen: u64 = 0;
		for key in pduid_pdu.rev_raw_keys_blocking() {
			let key = key?;
			seen = seen.wrapping_add(1);
			if seen.is_multiple_of(SCAN_SHUTDOWN_CHECK_EVERY) {
				server.check_running()?;
			}
			if let Some(count) = decode_normal_count(&key) {
				if count > max {
					max = count;
				}
			}
		}
		Ok(max)
	}
}

/// Decode a `pduid_pdu` key's count when it is `PduCount::Normal`. Returns
/// `None` for Backfilled keys or keys of unrecognized length. Key layout
/// (see `src/core/matrix/pdu/raw_id.rs`):
///   Normal:     [shortroomid:u64 BE][count:u64 BE]                  = 16 bytes
///   Backfilled: [shortroomid:u64 BE][0_u64 BE][count:i64 BE as u64] = 24 bytes
fn decode_normal_count(key: &[u8]) -> Option<u64> {
	const NORMAL_LEN: usize = size_of::<u64>() + size_of::<u64>();
	const BACKFILLED_LEN: usize = size_of::<u64>() + size_of::<u64>() + size_of::<i64>();
	if key.len() != NORMAL_LEN && key.len() != BACKFILLED_LEN {
		return None;
	}
	match RawPduId::from(key).pdu_count() {
		| PduCount::Normal(n) => Some(n),
		| PduCount::Backfilled(_) => None,
	}
}

impl Data {
	pub fn bump_database_version(&self, new_version: u64) {
		self.global.raw_put(b"version", new_version);
	}

	pub async fn database_version(&self) -> u64 {
		self.global
			.get(b"version")
			.await
			.deserialized()
			.unwrap_or(0)
	}
}
