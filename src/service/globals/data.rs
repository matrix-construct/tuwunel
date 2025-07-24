use std::sync::Arc;

use tuwunel_core::{
	Result, utils,
	utils::two_phase_counter::{Counter as TwoPhaseCounter, Permit as TwoPhasePermit},
};
use tuwunel_database::{Database, Deserialized, Map};

pub struct Data {
	global: Arc<Map>,
	counter: Arc<Counter>,
	pub(super) db: Arc<Database>,
}

pub(super) type Permit = TwoPhasePermit<Callback>;
type Counter = TwoPhaseCounter<Callback>;
type Callback = Box<dyn Fn(u64) -> Result + Send + Sync>;

const COUNTER: &[u8] = b"c";

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let db = args.db.clone();
		Self {
			db: args.db.clone(),
			global: args.db["global"].clone(),
			counter: Counter::new(
				Self::stored_count(&args.db["global"]).expect("initialized global counter"),
				Box::new(move |count| Self::store_count(&db, &db["global"], count)),
			),
		}
	}

	#[inline]
	pub fn next_count(&self) -> Permit {
		self.counter
			.next()
			.expect("failed to obtain next sequence number")
	}

	#[inline]
	pub fn current_count(&self) -> u64 { self.counter.current() }

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
