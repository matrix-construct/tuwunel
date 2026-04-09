#![allow(unused_features)] // 1.96.0-nightly 2026-03-07 bug

extern crate rust_rocksdb as rocksdb;

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}
tuwunel_core::rustc_flags_capture! {}

mod cork;
mod de;
mod deserialized;
mod engine;
mod handle;
pub mod keyval;
mod map;
pub mod maps;
mod pool;
mod ser;
mod stream;
#[cfg(test)]
mod tests;
pub(crate) mod util;

use std::{ops::Index, sync::Arc};

use log as _;
use tuwunel_core::{Result, Server, err};

pub use self::{
	de::{Ignore, IgnoreAll},
	deserialized::Deserialized,
	engine::{
		FRAME_HEADER_LEN, FRAME_TYPE_DATA, FRAME_TYPE_HEARTBEAT, WalFrame, is_wal_gap_error,
	},
	handle::Handle,
	keyval::{KeyVal, Slice, serialize_key, serialize_val},
	map::{Get, Map, Qry, compact},
	ser::{Cbor, Interfix, Json, SEP, Separator, serialize, serialize_to, serialize_to_vec},
};
pub(crate) use self::{
	engine::{Engine, context::Context},
	util::or_else,
};
use crate::maps::{Maps, MapsKey, MapsVal};

pub struct Database {
	maps: Maps,
	pub engine: Arc<Engine>,
	pub(crate) _ctx: Arc<Context>,
}

impl Database {
	/// Load an existing database or create a new one.
	pub async fn open(server: &Arc<Server>) -> Result<Arc<Self>> {
		let ctx = Context::new(server)?;
		let engine = Engine::open(ctx.clone(), maps::MAPS).await?;
		Ok(Arc::new(Self {
			maps: maps::open(&engine)?,
			engine: engine.clone(),
			_ctx: ctx,
		}))
	}

	#[inline]
	pub fn get(&self, name: &str) -> Result<&Arc<Map>> {
		self.maps
			.get(name)
			.ok_or_else(|| err!(Request(NotFound("column not found"))))
	}

	#[inline]
	pub fn iter(&self) -> impl Iterator<Item = (&MapsKey, &MapsVal)> + Send + '_ {
		self.maps.iter()
	}

	#[inline]
	pub fn keys(&self) -> impl Iterator<Item = &MapsKey> + Send + '_ { self.maps.keys() }

	#[inline]
	#[must_use]
	pub fn is_read_only(&self) -> bool { self.engine.is_read_only() }

	#[inline]
	#[must_use]
	pub fn is_secondary(&self) -> bool { self.engine.is_secondary() }

	/// Returns the primary's current latest WAL sequence number.
	///
	/// Used by replication status endpoints and heartbeat frames.
	#[inline]
	#[must_use]
	pub fn latest_wal_sequence(&self) -> u64 { self.engine.latest_wal_sequence() }

	/// Return a WAL frame iterator starting at `since`.
	///
	/// See `Engine::wal_frame_iter` for semantics. Returns `Err` if `since`
	/// is older than the oldest retained WAL segment.
	pub fn wal_frame_iter(
		&self,
		since: u64,
	) -> Result<Box<dyn Iterator<Item = Result<WalFrame>> + Send>> {
		self.engine.wal_frame_iter(since)
	}

	/// Create a RocksDB checkpoint at `dest`.
	///
	/// Returns the WAL sequence number at checkpoint creation time.
	pub fn create_checkpoint(&self, dest: &std::path::Path) -> Result<u64> {
		self.engine.create_checkpoint(dest)
	}

	/// Apply a raw WriteBatch (from the primary's WAL stream) to this database.
	///
	/// Used by the secondary replication worker to replay incoming batches.
	pub fn write_raw_batch(&self, data: &[u8]) -> Result {
		use rocksdb::{WriteBatch, WriteOptions};
		let batch = WriteBatch::from_data(data);
		let opts = WriteOptions::default();
		self.engine
			.db
			.write_opt(&batch, &opts)
			.map_err(util::map_err)
	}

	/// Read the secondary's persisted WAL resume cursor from the
	/// `replication_meta` column family.
	///
	/// Returns `Ok(0)` when no cursor has been written yet (fresh secondary).
	pub fn get_replication_resume_seq(&self) -> Result<u64> {
		use tuwunel_core::utils::result::NotFound;

		let map = &self["replication_meta"];
		let result = map.get_blocking(b"primary_resume_seq");
		if result.is_not_found() {
			return Ok(0);
		}
		let handle = result?;
		if handle.len() >= 8 {
			Ok(u64::from_le_bytes(handle[..8].try_into().expect("8 bytes")))
		} else {
			Ok(0)
		}
	}

	/// Persist the secondary's WAL resume cursor to the `replication_meta`
	/// column family so it survives restarts.
	pub fn set_replication_resume_seq(&self, seq: u64) -> Result {
		let map = &self["replication_meta"];
		map.insert(b"primary_resume_seq", seq.to_le_bytes());
		Ok(())
	}
}

impl Index<&str> for Database {
	type Output = Arc<Map>;

	fn index(&self, name: &str) -> &Self::Output {
		self.maps
			.get(name)
			.expect("column in database does not exist")
	}
}
