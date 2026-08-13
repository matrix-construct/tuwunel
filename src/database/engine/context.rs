use std::{
	collections::BTreeMap,
	fs::remove_dir_all,
	path::Path,
	sync::{Arc, Mutex},
};

use rocksdb::{Cache, LruCacheOptions};
use tuwunel_core::{
	Result, Server, debug,
	utils::{math::usize_from_f64, result::LogErr},
};

use super::env::Env;
use crate::pool::Pool;

/// One block-cache pool, plus the column families participating in it.
///
/// Pools may be shared by multiple CFs (`SHARED_POOL`, symmetric
/// `CacheDisp::SharedWith` pairs); the participant list lets the admin
/// surface name them.
pub(crate) struct ColCache {
	pub(crate) cache: Cache,

	pub(crate) participants: Vec<&'static str>,
}

/// Holds shared resources that must outlive an opened database.
///
/// The worker pool, caches, server handle, and RocksDB environment are prepared
/// before the database opens. Keeping them in one shared context gives every
/// engine component a common owner for those resources.
pub(crate) struct Context {
	pub(crate) pool: Arc<Pool>,

	/// Retained because rust-rocksdb's `Cache` binding lacks `get_capacity`.
	pub(crate) row_cache_capacity: usize,

	pub(crate) row_cache: Mutex<Cache>,

	pub(crate) col_cache: Mutex<ColCaches>,

	pub(crate) server: Arc<Server>,

	pub(super) env: Arc<Env>,
}

/// Map of block-cache pools keyed by pool name. The pool name is either
/// `SHARED_POOL` or the first-arrival CF that created it.
pub(crate) type ColCaches = BTreeMap<&'static str, ColCache>;

/// Name under which the shared block cache (every CF with
/// `CacheDisp::Shared`) is registered in [`Context::col_cache`].
pub(crate) const SHARED_POOL: &str = "Shared";

impl Context {
	pub(crate) fn new(server: &Arc<Server>) -> Result<Arc<Self>> {
		let config = &server.config;
		let cache_capacity_bytes = config.db_cache_capacity_mb * 1024.0 * 1024.0;

		let col_shard_bits = 7;
		let col_cache_capacity_bytes = usize_from_f64(cache_capacity_bytes * 0.50)?;

		let row_shard_bits = 7;
		let row_cache_capacity_bytes = usize_from_f64(cache_capacity_bytes * 0.50)?;

		let mut row_cache_opts = LruCacheOptions::default();
		row_cache_opts.set_num_shard_bits(row_shard_bits);
		row_cache_opts.set_capacity(row_cache_capacity_bytes);
		let row_cache = Cache::new_lru_cache_opts(&row_cache_opts);

		let mut col_cache_opts = LruCacheOptions::default();
		col_cache_opts.set_num_shard_bits(col_shard_bits);
		col_cache_opts.set_capacity(col_cache_capacity_bytes);
		let col_cache = Cache::new_lru_cache_opts(&col_cache_opts);
		let shared = ColCache {
			cache: col_cache,
			participants: Vec::new(),
		};
		let col_cache: ColCaches = [(SHARED_POOL, shared)].into();

		Ok(Arc::new(Self {
			pool: Pool::new(server)?,
			row_cache_capacity: row_cache_capacity_bytes,
			row_cache: row_cache.into(),
			col_cache: col_cache.into(),
			server: server.clone(),
			env: Env::acquire(server)?,
		}))
	}
}

impl Drop for Context {
	#[cold]
	fn drop(&mut self) {
		debug!("Closing frontend pool");
		self.pool.close();

		after_close(self, &self.server.config.database_path)
			.expect("Failed to execute after_close handler");
	}
}

/// For unit and integration tests the 'fresh' directive deletes found db.
pub(super) fn before_open(ctx: &Arc<Context>, path: &Path) -> Result {
	if ctx.server.config.test.contains("fresh") {
		match delete_database_for_testing(ctx, path) {
			| Err(e) if !e.is_not_found() => return Err(e),
			| _ => (),
		}
	}

	Ok(())
}

/// For unit and integration tests the 'cleanup' directive deletes after close
/// to cleanup.
fn after_close(ctx: &Context, path: &Path) -> Result {
	if ctx.server.config.test.contains("cleanup") {
		delete_database_for_testing(ctx, path)
			.log_err()
			.ok();
	}

	Ok(())
}

/// For unit and integration tests; removes the database directory when called.
/// To prevent misuse, cfg!(test) must be true for a unit test or the
/// integration test server is named localhost.
#[tracing::instrument(level = "debug", skip_all)]
fn delete_database_for_testing(ctx: &Context, path: &Path) -> Result {
	let config = &ctx.server.config;
	let localhost = config
		.server_name
		.as_str()
		.starts_with("localhost");

	if !cfg!(test) && !localhost {
		return Ok(());
	}

	debug_assert!(
		config.test.contains("cleanup") | config.test.contains("fresh"),
		"missing any test directive legitimating this call.",
	);

	remove_dir_all(path).map_err(Into::into)
}
