mod clear;
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
pub use self::{get_batch::Get, qry_batch::Qry};
use crate::Engine;

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

	#[inline]
	pub fn property_integer(&self, name: &CStr) -> Result<u64> {
		self.engine.property_integer(&self.cf(), name)
	}

	#[inline]
	pub fn property(&self, name: &str) -> Result<String> {
		self.engine.property(&self.cf(), name)
	}

	#[inline]
	pub fn name(&self) -> &str { self.name }

	#[inline]
	pub(crate) fn engine(&self) -> &Arc<Engine> { &self.engine }

	#[inline]
	pub(crate) fn cf(&self) -> impl AsColumnFamilyRef + '_ { &*self.cf }
}

impl Debug for Map {
	fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(out, "Map {{name: {0}}}", self.name)
	}
}

impl Display for Map {
	fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result { write!(out, "{0}", self.name) }
}
