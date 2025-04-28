mod clear;
pub mod compact;
mod contains;
mod count;
mod get;
mod get_batch;
mod insert;
mod keys;
mod keys_from;
mod keys_prefix;
mod open;
mod options;
mod qry;
mod qry_batch;
mod remove;
mod rev_keys;
mod rev_keys_from;
mod rev_keys_prefix;
mod rev_stream;
mod rev_stream_from;
mod rev_stream_prefix;
mod stream;
mod stream_from;
mod stream_prefix;

use std::{
	convert::AsRef,
	ffi::CStr,
	fmt,
	fmt::{Debug, Display},
	future::Future,
	pin::Pin,
	sync::Arc,
};

use rocksdb::{AsColumnFamilyRef, ColumnFamily, ReadOptions, WriteOptions};
use tuwunel_core::Result;

pub(crate) use self::options::{
	cache_iter_options_default, cache_read_options_default, iter_options_default,
	read_options_default, write_options_default,
};
pub use self::{get_batch::Get, qry_batch::Qry};
use crate::{Engine, watchers::Watchers};

pub struct Map {
	name: &'static str,
	watchers: Watchers,
	cf: Arc<ColumnFamily>,
	db: Arc<Engine>,
	read_options: ReadOptions,
	cache_read_options: ReadOptions,
	write_options: WriteOptions,
}

impl Map {
	pub(crate) fn open(db: &Arc<Engine>, name: &'static str) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			name,
			watchers: Watchers::default(),
			cf: open::open(db, name),
			db: db.clone(),
			read_options: read_options_default(db),
			cache_read_options: cache_read_options_default(db),
			write_options: write_options_default(db),
		}))
	}

	#[inline]
	pub fn watch_prefix<'a, K>(
		&'a self,
		prefix: &K,
	) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
	where
		K: AsRef<[u8]> + ?Sized + Debug,
	{
		self.watchers.watch(prefix.as_ref())
	}

	#[inline]
	pub fn property_integer(&self, name: &CStr) -> Result<u64> {
		self.db.property_integer(&self.cf(), name)
	}

	#[inline]
	pub fn property(&self, name: &str) -> Result<String> {
		self.db.property(&self.cf(), name)
	}

	#[inline]
	pub fn name(&self) -> &str {
		self.name
	}

	#[inline]
	pub(crate) fn db(&self) -> &Arc<Engine> {
		&self.db
	}

	#[inline]
	pub(crate) fn cf(&self) -> impl AsColumnFamilyRef + '_ {
		&*self.cf
	}
}

impl Debug for Map {
	fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(out, "Map {{name: {0}}}", self.name)
	}
}

impl Display for Map {
	fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(out, "{0}", self.name)
	}
}
