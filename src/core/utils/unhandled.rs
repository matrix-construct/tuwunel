//! Indicate a branch which will never be taken. Currently expands to
//! `unimplemented!()` in every build; the `unreachable_unchecked()` arm below
//! is parked behind `#[cfg(disable)]` until callsites are vetted.

#[cfg(disable)] // activate when more stable and callsites are vetted.
// #[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! unhandled {
	($msg:literal) => {
		// SAFETY: Eliminates branches never encountered in the codebase. This can
		// promote optimization and reduce codegen. The developer must verify for every
		// invoking callsite that the unhandled type is in no way involved and could not
		// possibly be encountered.
		unsafe {
			std::hint::unreachable_unchecked();
		}
	};
}

//#[cfg(debug_assertions)]
#[macro_export]
macro_rules! unhandled {
	($msg:literal) => {
		$crate::maybe_unhandled!($msg)
	};
}

#[macro_export]
macro_rules! maybe_unhandled {
	($msg:literal) => {
		unimplemented!($msg)
	};
}
