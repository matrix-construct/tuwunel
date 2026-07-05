use std::{convert::AsRef, fmt::Debug};

use tuwunel_core::{Result, implement};

use crate::util::or_else;

#[implement(super::Map)]
#[tracing::instrument(skip(self, key), fields(%self), level = "trace")]
pub fn remove<K>(&self, key: &K)
where
	K: AsRef<[u8]> + ?Sized + Debug,
{
	self.remove_fallible(key)
		.expect("database remove error");
}

#[implement(super::Map)]
#[tracing::instrument(skip(self, key), fields(%self), level = "trace")]
pub fn remove_fallible<K>(&self, key: &K) -> Result
where
	K: AsRef<[u8]> + ?Sized + Debug,
{
	let write_options = &self.write_options;
	self.engine
		.db
		.delete_cf_opt(&self.cf(), key, write_options)
		.or_else(or_else)?;

	if !self.engine.corked() {
		self.engine.flush()?;
	}

	self.notify(key.as_ref());
	Ok(())
}
