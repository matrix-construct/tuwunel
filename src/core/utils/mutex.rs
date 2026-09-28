//! Poison-tolerant locking for std mutexes.
//!
//! `Mutex::lock` reports an error once any holder has panicked; the extension
//! here takes the lock regardless.

use std::sync::{Mutex, MutexGuard, PoisonError};

#[cfg(test)]
mod tests;

/// Locking for a std mutex whose data no holder can leave half-updated.
///
/// For such data the poison carries no information, so code that must not
/// panic, such as a drop running during an unwind, can still take the lock.
pub trait MutexExt<T: ?Sized> {
	/// Take the lock, adopting a poisoned one.
	///
	/// The poison only records that some holder panicked, so adopting it is
	/// correct only where no holder can panic midway through an update. A
	/// thread relocking a mutex it already holds may still panic or deadlock,
	/// as under `lock`.
	fn lock_adopting(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> MutexExt<T> for Mutex<T> {
	#[inline]
	fn lock_adopting(&self) -> MutexGuard<'_, T> {
		self.lock()
			.unwrap_or_else(PoisonError::into_inner)
	}
}
