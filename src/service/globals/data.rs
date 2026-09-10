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

/// High-water mark written by a graceful shutdown. Its presence means the
/// counter was persisted in full and `pduid_pdu` need not be scanned; its
/// absence means the process stopped without recording one and the scan is
/// required. Cleared as soon as it is consumed at startup.
const CLEAN_COUNTER: &[u8] = b"clean_c";

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let db = args.db.clone();
		let global = args.db["global"].clone();

		// `global['c']` is cork-buffered and can lag behind durable `pduid_pdu`
		// writes; on promote-restart, derive the high-water mark from both so
		// the new primary doesn't re-issue colliding counts.
		let from_global_c = Self::stored_count(&global).expect("initialize global counter");

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
		let from_clean = Self::stored_clean_count(&global);
		let from_pdus = match from_clean {
			| Some(clean) => {
				info!(clean, "Clean shutdown recorded: skipping PDU high-water scan.");
				clean
			},
			| None => {
				warn!(
					"No clean shutdown was recorded. Recovering the PDU high-water mark by \
					 scanning pduid_pdu; this is proportional to database size and the \
					 listener stays unbound until it completes."
				);
				Self::max_pdu_count_across_rooms(&args.db["pduid_pdu"])
					.expect("recover pdu high-water mark")
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

		let count = from_global_c.max(from_pdus);
		let retires = Sender::new(count);
		Self {
			db: args.db.clone(),
			global: args.db["global"].clone(),
			retires: retires.clone(),
			counter: Counter::new(
				count,
				Box::new(move |count| Self::store_count(&db, &db["global"], count)),
				Box::new(move |count| Self::handle_retire(&retires, count)),
			),
		}
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
	pub(super) fn persist_clean_shutdown(&self) -> u64 {
		let count = self.counter.dispatched();
		let _cork = self.db.cork_and_sync();
		self.global.insert(CLEAN_COUNTER, count.to_be_bytes());

		count
	}

	/// Largest `Normal` PDU count across `pduid_pdu`. Backfilled counts are
	/// ignored (not drawn from the global counter). Returns 0 if empty.
	///
	/// Reads every key in the column family, so this is O(total events) and
	/// takes ~60 minutes on a 12 GB / 868M-event database. It cannot be made
	/// cheap — every iterator here runs with `total_order_seek`, so seeking
	/// per-room costs more index reads than stepping does — which is why the
	/// caller avoids reaching it rather than optimising it.
	fn max_pdu_count_across_rooms(pduid_pdu: &Arc<Map>) -> Result<u64> {
		let mut max: u64 = 0;
		for key in pduid_pdu.rev_raw_keys_blocking() {
			let key = key?;
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
