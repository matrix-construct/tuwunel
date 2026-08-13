//! Persistent storage primitives for Tuwunel.
//!
//! The crate wraps RocksDB with typed maps, serialization helpers, and atomic
//! transactions. Database handles expose the configured column families and the
//! engine that owns them.

#![deny(missing_docs)]

extern crate rust_rocksdb as rocksdb;

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}
tuwunel_core::rustc_flags_capture! {}

mod cork;
mod de;
mod deserialized;
mod engine;
mod handle;
/// Serialized database key and value types.
///
/// The aliases distinguish keys from values while sharing compact byte buffers.
/// Helpers encode typed components into the byte representation used by
/// database maps.
pub mod keyval;
mod map;
/// Column family descriptors used by the homeserver database.
///
/// The catalog names every configured map and supplies its RocksDB tuning
/// profile. Database startup opens these descriptors as one coherent
/// collection.
pub mod maps;
mod pool;
mod ser;
mod stream;
#[cfg(test)]
mod tests;
mod txn;
pub(crate) mod util;

use std::{ops::Index, sync::Arc};

use log as _;
use tuwunel_core::{Result, Server, err};

pub use self::{
	cork::Cork,
	de::{Ignore, IgnoreAll, from_slice as deserialize_from_slice},
	deserialized::Deserialized,
	engine::Engine,
	handle::Handle,
	keyval::{KeyBuf, KeyVal, Slice, serialize_key, serialize_val},
	map::{Get, Map, Qry, compact},
	ser::{Cbor, Interfix, Json, SEP, Separator, serialize, serialize_to, serialize_to_vec},
	txn::Txn,
};
pub(crate) use self::{engine::context::Context, util::or_else};
use crate::maps::{Maps, MapsKey, MapsVal, open as open_maps};

/// An open Tuwunel database and its configured maps.
///
/// Each instance owns maps created by one RocksDB engine. Typed accessors
/// preserve that ownership relationship for individual reads and atomic
/// transactions.
pub struct Database {
	maps: Maps,
	/// The RocksDB engine backing every map in this database.
	///
	/// Callers use the engine for database-wide operations such as backups and
	/// memory reporting. Maps and transactions must remain associated with
	/// this same engine.
	pub engine: Arc<Engine>,
	pub(crate) _ctx: Arc<Context>,
}

impl Database {
	/// Loads an existing database or creates a new one.
	///
	/// The configured map catalog is opened after the engine and indexed by
	/// column family identity. The returned shared handle keeps the engine
	/// context alive for its full lifetime.
	pub async fn open(server: &Arc<Server>) -> Result<Arc<Self>> {
		let ctx = Context::new(server)?;
		let engine = Engine::open(ctx.clone(), maps::MAPS).await?;
		let maps = open_maps(&engine)?;
		let cf_index = maps
			.values()
			.map(|map| (map.cf_id(), Arc::downgrade(map)))
			.collect();

		engine.set_cf_index(cf_index);

		Ok(Arc::new(Self { maps, engine, _ctx: ctx }))
	}

	#[inline]
	/// Creates an empty transaction for this database.
	///
	/// The transaction is bound to this database's engine and accepts writes
	/// only for maps owned by that engine. Queued operations remain unapplied
	/// until the transaction is executed.
	pub fn txn(&self) -> Txn { Txn::new(&self.engine) }

	#[inline]
	/// Retrieves a configured map by name.
	///
	/// The returned map belongs to this database's engine. An unknown name
	/// produces a not-found database error.
	pub fn get(&self, name: &str) -> Result<&Arc<Map>> {
		self.maps
			.get(name)
			.ok_or_else(|| err!(Request(NotFound("column not found"))))
	}

	/// Opens an existing column family outside the configured map catalog.
	///
	/// Migration readers use this for foreign database families that are not
	/// described by `MAPS`. An absent family returns `None` without creating
	/// it.
	pub fn open_cf(&self, name: &'static str) -> Result<Option<Arc<Map>>> {
		self.engine
			.has_cf(name)
			.then(|| Map::open(&self.engine, name))
			.transpose()
	}

	#[inline]
	/// Iterates over configured map names and handles.
	///
	/// Entries follow the catalog's sorted map order. Every yielded handle
	/// belongs to this database's engine.
	pub fn iter(&self) -> impl Iterator<Item = (&MapsKey, &MapsVal)> + Send + '_ {
		self.maps.iter()
	}

	#[inline]
	/// Iterates over the configured map names.
	///
	/// Names follow the catalog's sorted map order. The iterator borrows this
	/// database for the duration of the traversal.
	pub fn keys(&self) -> impl Iterator<Item = &MapsKey> + Send + '_ { self.maps.keys() }

	#[inline]
	#[must_use]
	/// Reports whether the engine rejects writes.
	///
	/// Writes are rejected when the database is opened read-only or as a
	/// secondary instance. The value applies to every map owned by this
	/// database.
	pub fn is_read_only(&self) -> bool { self.engine.is_read_only() }

	#[inline]
	#[must_use]
	/// Reports whether this database is a secondary RocksDB instance.
	///
	/// A secondary instance follows another database and does not act as its
	/// primary writer. The value applies to every map owned by this database.
	pub fn is_secondary(&self) -> bool { self.engine.is_secondary() }
}

impl Index<&str> for Database {
	type Output = Arc<Map>;

	/// Retrieves a configured map by name.
	///
	/// Indexing offers concise access when the map name is a static database
	/// invariant. Use [`Database::get`] when absence should be handled as an
	/// error.
	///
	/// # Panics
	///
	/// Panics if this database has no configured map with the requested name.
	fn index(&self, name: &str) -> &Self::Output {
		self.maps
			.get(name)
			.expect("column in database does not exist")
	}
}
