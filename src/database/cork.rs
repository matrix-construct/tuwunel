use std::sync::Arc;

use crate::{Database, Engine};

pub struct Cork {
	db: Arc<Engine>,
	flush: bool,
	sync: bool,
}

impl Database {
	#[inline]
	#[must_use]
	pub fn cork(&self) -> Cork {
		Cork::new(&self.db, false, false)
	}

	#[inline]
	#[must_use]
	pub fn cork_and_flush(&self) -> Cork {
		Cork::new(&self.db, true, false)
	}

	#[inline]
	#[must_use]
	pub fn cork_and_sync(&self) -> Cork {
		Cork::new(&self.db, true, true)
	}
}

impl Cork {
	#[inline]
	pub(super) fn new(db: &Arc<Engine>, flush: bool, sync: bool) -> Self {
		db.cork();
		Self { db: db.clone(), flush, sync }
	}
}

impl Drop for Cork {
	fn drop(&mut self) {
		self.db.uncork();
		if self.flush {
			self.db.flush().ok();
		}
		if self.sync {
			self.db.sync().ok();
		}
	}
}
