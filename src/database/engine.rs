mod backup;
mod cf_opts;
pub(crate) mod context;
mod db_opts;
pub(crate) mod descriptor;
mod files;
mod logger;
mod memory_usage;
mod open;
mod repair;

use std::{
	ffi::CStr,
	sync::{
		Arc,
		atomic::{AtomicU32, Ordering},
	},
};

use rocksdb::{
	AsColumnFamilyRef, BoundColumnFamily, DBCommon, DBWithThreadMode, MultiThreaded,
	WaitForCompactOptions,
};
use tuwunel_core::{Err, Result, debug, info, warn};

use crate::{
	Context,
	pool::Pool,
	util::{map_err, result},
};

pub struct Engine {
	pub(crate) db: Db,
	pub(crate) pool: Arc<Pool>,
	pub(crate) ctx: Arc<Context>,
	pub(super) read_only: bool,
	pub(super) secondary: bool,
	pub(crate) checksums: bool,
	corks: AtomicU32,
}

pub(crate) type Db = DBWithThreadMode<MultiThreaded>;

impl Engine {
	#[tracing::instrument(
		level = "info",
		skip_all,
		fields(
			sequence = ?self.current_sequence(),
		),
	)]
	pub fn wait_compactions_blocking(&self) -> Result {
		let mut opts = WaitForCompactOptions::default();
		opts.set_abort_on_pause(true);
		opts.set_flush(false);
		opts.set_timeout(0);

		self.db.wait_for_compact(&opts).map_err(map_err)
	}

	#[tracing::instrument(
		level = "info",
		skip_all,
		fields(
			sequence = ?self.current_sequence(),
		),
	)]
	pub fn sort(&self) -> Result {
		let flushoptions = rocksdb::FlushOptions::default();
		result(DBCommon::flush_opt(&self.db, &flushoptions))
	}

	#[tracing::instrument(
		level = "debug",
		skip_all,
		fields(
			sequence = ?self.current_sequence(),
		),
	)]
	pub fn update(&self) -> Result {
		self.db
			.try_catch_up_with_primary()
			.map_err(map_err)
	}

	#[tracing::instrument(level = "info", skip_all)]
	pub fn sync(&self) -> Result {
		result(DBCommon::flush_wal(&self.db, true))
	}

	#[tracing::instrument(level = "debug", skip_all)]
	pub fn flush(&self) -> Result {
		result(DBCommon::flush_wal(&self.db, false))
	}

	#[inline]
	pub(crate) fn cork(&self) {
		self.corks.fetch_add(1, Ordering::Relaxed);
	}

	#[inline]
	pub(crate) fn uncork(&self) {
		self.corks.fetch_sub(1, Ordering::Relaxed);
	}

	#[inline]
	pub fn corked(&self) -> bool {
		self.corks.load(Ordering::Relaxed) > 0
	}

	/// Query for database property by null-terminated name which is expected to
	/// have a result with an integer representation. This is intended for
	/// low-overhead programmatic use.
	pub(crate) fn property_integer(
		&self,
		cf: &impl AsColumnFamilyRef,
		name: &CStr,
	) -> Result<u64> {
		result(self.db.property_int_value_cf(cf, name))
			.and_then(|val| val.map_or_else(|| Err!("Property {name:?} not found."), Ok))
	}

	/// Query for database property by name receiving the result in a string.
	pub(crate) fn property(&self, cf: &impl AsColumnFamilyRef, name: &str) -> Result<String> {
		result(self.db.property_value_cf(cf, name))
			.and_then(|val| val.map_or_else(|| Err!("Property {name:?} not found."), Ok))
	}

	pub(crate) fn cf(&self, name: &str) -> Arc<BoundColumnFamily<'_>> {
		self.db
			.cf_handle(name)
			.expect("column must be described prior to database open")
	}

	#[inline]
	#[must_use]
	#[tracing::instrument(name = "sequence", level = "debug", skip_all, fields(sequence))]
	pub fn current_sequence(&self) -> u64 {
		let sequence = self.db.latest_sequence_number();

		#[cfg(debug_assertions)]
		tracing::Span::current().record("sequence", sequence);

		sequence
	}

	#[inline]
	#[must_use]
	pub fn is_read_only(&self) -> bool {
		self.secondary || self.read_only
	}

	#[inline]
	#[must_use]
	pub fn is_secondary(&self) -> bool {
		self.secondary
	}
}

impl Drop for Engine {
	#[cold]
	fn drop(&mut self) {
		const BLOCKING: bool = true;

		debug!("Waiting for background tasks to finish...");
		self.db.cancel_all_background_work(BLOCKING);

		info!(
			sequence = %self.current_sequence(),
			"Closing database..."
		);
	}
}
