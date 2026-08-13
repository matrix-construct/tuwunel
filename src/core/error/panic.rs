use std::{
	any::Any,
	panic::{RefUnwindSafe, UnwindSafe, panic_any},
};

use super::Error;
use crate::debug;

impl UnwindSafe for Error {}
impl RefUnwindSafe for Error {}

impl Error {
	/// Starts a panic using the boxed value derived from this error.
	///
	/// Explicit panic variants and task joins that ended by panicking supply
	/// their stored boxed value. Other errors are boxed as typed values.
	///
	/// # Panics
	///
	/// Always panics by design.
	#[inline]
	pub fn panic(self) -> ! { panic_any(self.into_panic()) }

	/// Wraps a caught panic payload as an error.
	///
	/// A static string message is extracted when the payload exposes one. The
	/// original payload remains available for later unwinding.
	#[must_use]
	#[inline]
	pub fn from_panic(e: Box<dyn Any + Send + 'static>) -> Self {
		Self::Panic(debug::panic_str(&e), e.into())
	}

	/// Converts this error into a boxed value for panicking.
	///
	/// Explicit panic variants and task joins that ended by panicking yield
	/// their stored boxed value. Other variants are boxed as typed values.
	///
	/// # Panics
	///
	/// Panics if a stored panic payload's mutex is poisoned or a task join was
	/// cancelled instead of ending with a panic.
	#[inline]
	pub fn into_panic(self) -> Box<dyn Any + Send> {
		match self {
			| Self::JoinError(e) => e.into_panic(),
			| Self::Panic(_, e) | Self::PanicAny(e) =>
				e.into_inner().expect("Error contained panic"),
			| _ => Box::new(self),
		}
	}

	/// Extracts a static message from a carried panic payload.
	///
	/// Non-panic errors return `None`. Unsupported payload representations
	/// produce an empty string, and the error is consumed while inspecting the
	/// payload.
	///
	/// # Panics
	///
	/// Panics if a stored panic payload's mutex is poisoned.
	#[inline]
	pub fn panic_str(self) -> Option<&'static str> {
		self.is_panic().then(|| {
			let panic = self.into_panic();
			debug::panic_str(&panic)
		})
	}

	/// Tests whether this error carries a panic payload.
	///
	/// Explicit panic variants always match. A task-join error matches only
	/// when the joined task ended by panicking.
	#[inline]
	pub fn is_panic(&self) -> bool {
		match &self {
			| Self::JoinError(e) => e.is_panic(),
			| Self::Panic(..) | Self::PanicAny(..) => true,
			| _ => false,
		}
	}
}
