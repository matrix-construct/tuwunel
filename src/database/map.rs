mod clear;
/// Manual compaction controls for database maps.
///
/// The module exposes range, level, completion, and exclusivity settings.
/// `Map::compact_blocking` applies them synchronously.
pub mod compact;
mod contains;
mod count;
mod del;
mod del_prefix;
mod get;
mod get_batch;
mod insert;
mod keys;
mod keys_from;
mod keys_prefix;
mod open;
mod options;
mod put;
mod qry;
mod qry_batch;
mod remove;
mod rev_keys;
mod rev_keys_from;
mod rev_keys_prefix;
mod rev_stream;
mod rev_stream_from;
mod rev_stream_prefix;
mod seek;
mod stream;
mod stream_from;
mod stream_prefix;
mod watch;

use std::{
	ffi::CStr,
	fmt,
	fmt::{Debug, Display},
	sync::Arc,
};

use rocksdb::{AsColumnFamilyRef, ColumnFamily, ReadOptions, WriteOptions};
use tuwunel_core::Result;

pub(crate) use self::options::{
	cache_iter_options_default, cache_read_options_default, iter_options_default,
	read_options_default, write_options_default,
};
use self::watch::Watch;
/// Stream extensions for batched map reads.
///
/// `Get` accepts raw keys, while `Qry` serializes structured keys before
/// lookup. Both yield pinned value handles through an asynchronous stream.
pub use self::{get_batch::Get, qry_batch::Qry};
use crate::Engine;

/// Provides typed and raw access to one RocksDB column family.
///
/// A map retains its column-family handle and the engine that owns it. Point
/// operations reuse read and write options prepared when the map opens.
pub struct Map {
	name: &'static str,
	watch: Watch,
	cf: Arc<ColumnFamily>,
	engine: Arc<Engine>,
	read_options: ReadOptions,
	cache_read_options: ReadOptions,
	write_options: WriteOptions,
}

impl Map {
	/// Opens a map for a named column family.
	///
	/// The returned map keeps the engine alive for at least as long as its
	/// column-family handle. Its read and write options are initialized from
	/// the engine configuration.
	pub(crate) fn open(engine: &Arc<Engine>, name: &'static str) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			name,
			watch: Watch::default(),
			cf: open::open(engine, name),
			engine: engine.clone(),
			read_options: read_options_default(engine),
			cache_read_options: cache_read_options_default(engine),
			write_options: write_options_default(engine),
		}))
	}

	/// Reads an integer RocksDB property for this map.
	///
	/// The property query is scoped to this map's column family. Engine errors
	/// are returned to the caller.
	#[inline]
	pub fn property_integer(&self, name: &CStr) -> Result<u64> {
		self.engine.property_integer(&self.cf(), name)
	}

	/// Reads a string RocksDB property for this map.
	///
	/// The property query is scoped to this map's column family. Engine errors
	/// are returned to the caller.
	#[inline]
	pub fn property(&self, name: &str) -> Result<String> {
		self.engine.property(&self.cf(), name)
	}

	/// Returns the column-family name of this map.
	///
	/// The name is fixed when the map opens and lives for the duration of the
	/// process.
	#[inline]
	pub fn name(&self) -> &str { self.name }

	/// Returns the engine that owns this map.
	///
	/// The borrowed `Arc` keeps the same identity used to open the
	/// column-family handle.
	#[inline]
	pub(crate) fn engine(&self) -> &Arc<Engine> { &self.engine }

	/// Returns this map's RocksDB column-family handle.
	///
	/// The handle remains valid because the map retains its owning engine.
	#[inline]
	pub(crate) fn cf(&self) -> impl AsColumnFamilyRef + '_ { &*self.cf }

	/// Returns the numeric RocksDB identifier for this column family.
	///
	/// The identifier belongs to this map's engine and must not be compared
	/// across engines.
	#[inline]
	pub(crate) fn cf_id(&self) -> u32 { self.cf().id() }
}

impl Debug for Map {
	fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(out, "Map {{name: {0}}}", self.name)
	}
}

impl Display for Map {
	fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result { write!(out, "{0}", self.name) }
}
