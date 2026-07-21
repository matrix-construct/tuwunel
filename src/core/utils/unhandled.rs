//! Macros for branches expected never to execute.
//!
//! Active builds expand to `unimplemented!()`. The `unreachable_unchecked()`
//! alternative remains behind `#[cfg(disable)]` until every call site is
//! vetted.

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
