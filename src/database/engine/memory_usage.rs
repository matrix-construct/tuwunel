use std::{ffi::CStr, fmt::Write};

use rocksdb::perf::get_memory_usage_stats;
use tuwunel_core::{Result, implement};

use super::{
	Engine,
	context::{ColCache, SHARED_POOL},
};
use crate::or_else;

/// Per-CF byte count of the block cache the CF is using. Returns the same
/// value for every participant of a shared pool.
const CACHE_CAPACITY_PROPERTY: &CStr = c"rocksdb.block-cache-capacity";

fn mib(input: u64) -> f64 { f64::from(u32::try_from(input / 1024).unwrap_or(0)) / 1024.0 }

/// Formats the database engine's current memory usage.
///
/// The report covers memtables, pending writes, table readers, and the row
/// cache. When column-cache pools exist, it also includes their usage,
/// capacity, pinned memory, and participating column-family counts.
#[implement(Engine)]
pub fn memory_usage(&self) -> Result<String> {
	let mut res = String::new();
	let row_cache = self.ctx.row_cache.lock()?;
	let row_usage = u64::try_from(row_cache.get_usage())?;
	let row_capacity = u64::try_from(self.ctx.row_cache_capacity)?;
	let stats =
		get_memory_usage_stats(Some(&[&self.db]), Some(&[&*row_cache])).or_else(or_else)?;

	writeln!(res, "- Memory buffers: {:.2} MiB", mib(stats.mem_table_total))?;
	writeln!(res, "- Pending write:  {:.2} MiB", mib(stats.mem_table_unflushed))?;
	writeln!(res, "- Table readers:  {:.2} MiB", mib(stats.mem_table_readers_total))?;
	writeln!(
		res,
		"- Row cache:      {:.2} / {:.2} MiB ({:.1}%)",
		mib(row_usage),
		mib(row_capacity),
		utilization_percent(row_usage, row_capacity),
	)?;

	drop(row_cache);

	let pools = self.ctx.col_cache.lock()?;
	if pools.is_empty() {
		return Ok(res);
	}

	writeln!(res, "\n```")?;
	writeln!(
		res,
		"{:<34}  {:>11}  {:>14}  {:>8}  {:>12}  {:>3}",
		"POOL", "USAGE (MiB)", "CAPACITY (MiB)", "UTIL (%)", "PINNED (MiB)", "CFS",
	)?;

	for (name, pool) in &*pools {
		self.write_pool(&mut res, name, pool)?;
	}
	writeln!(res, "```")?;

	Ok(res)
}

#[implement(Engine)]
fn write_pool(&self, out: &mut String, name: &str, pool: &ColCache) -> Result {
	let label = if name == SHARED_POOL { "Shared" } else { name };
	let pinned = u64::try_from(pool.cache.get_pinned_usage())?;
	let usage = u64::try_from(pool.cache.get_usage())?;
	let capacity = pool
		.participants
		.first()
		.copied()
		.map(|cf_name| self.cf(cf_name))
		.and_then(|cf| {
			self.property_integer(&cf, CACHE_CAPACITY_PROPERTY)
				.ok()
		})
		.unwrap_or(0);

	writeln!(
		out,
		"{label:<34}  {:>11.2}  {:>14.2}  {:>8.1}  {:>12.2}  {:>3}",
		mib(usage),
		mib(capacity),
		utilization_percent(usage, capacity),
		mib(pinned),
		pool.participants.len(),
	)?;

	Ok(())
}

#[expect(clippy::as_conversions, clippy::cast_precision_loss)]
fn utilization_percent(usage: u64, capacity: u64) -> f64 {
	if capacity == 0 {
		return 0.0;
	}

	(usage as f64 / capacity as f64) * 100.0
}
