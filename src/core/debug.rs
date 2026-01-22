use std::{any::Any, env, panic, sync::LazyLock};

use tracing::Level;
// Export debug proc_macros
pub use tuwunel_macros::recursion_depth;

// Export all of the ancillary tools from here as well.
pub use crate::{result::DebugInspect, utils::debug::*};

/// Log event at given level in debug-mode (when debug-assertions are enabled).
/// In release-mode it becomes DEBUG level, and possibly subject to elision.
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! debug_event {
	( $level:expr_2021, $($x:tt)+ ) => {
		if $crate::debug::logging() {
			::tracing::event!( $level, _debug = true, $($x)+ )
		} else {
			::tracing::debug!( $($x)+ )
		}
	}
}

/// Log message at the ERROR level in debug-mode (when debug-assertions are
/// enabled). In release-mode it becomes DEBUG level, and possibly subject to
/// elision.
#[macro_export]
macro_rules! debug_error {
	( $($x:tt)+ ) => {
		$crate::debug_event!(::tracing::Level::ERROR, $($x)+ )
	}
}

/// Log message at the WARN level in debug-mode (when debug-assertions are
/// enabled). In release-mode it becomes DEBUG level, and possibly subject to
/// elision.
#[macro_export]
macro_rules! debug_warn {
	( $($x:tt)+ ) => {
		$crate::debug_event!(::tracing::Level::WARN, $($x)+ )
	}
}

/// Log message at the INFO level in debug-mode (when debug-assertions are
/// enabled). In release-mode it becomes DEBUG level, and possibly subject to
/// elision.
#[macro_export]
macro_rules! debug_info {
	( $($x:tt)+ ) => {
		$crate::debug_event!(::tracing::Level::INFO, $($x)+ )
	}
}

pub const INFO_SPAN_LEVEL: Level = if logging() { Level::INFO } else { Level::DEBUG };

pub static DEBUGGER: LazyLock<bool> =
	LazyLock::new(|| env::var("_").unwrap_or_default().ends_with("gdb"));

#[cfg_attr(debug_assertions, crate::ctor)]
#[cfg_attr(not(debug_assertions), allow(dead_code))]
fn set_panic_trap() {
	if !*DEBUGGER {
		return;
	}

	let next = panic::take_hook();
	panic::set_hook(Box::new(move |info| {
		panic_handler(info, &next);
	}));
}

#[cold]
#[inline(never)]
pub fn panic_handler(info: &panic::PanicHookInfo<'_>, next: &dyn Fn(&panic::PanicHookInfo<'_>)) {
	trap();
	next(info);
}

#[inline(always)]
pub fn trap() {
	#[cfg(core_intrinsics)]
	//SAFETY: embeds llvm intrinsic for hardware breakpoint
	unsafe {
		std::intrinsics::breakpoint();
	}

	#[cfg(all(not(core_intrinsics), target_arch = "x86_64"))]
	//SAFETY: embeds instruction for hardware breakpoint
	unsafe {
		std::arch::asm!("int3");
	}
}

#[must_use]
pub fn panic_str(p: &Box<dyn Any + Send + 'static>) -> &'static str {
	(**p)
		.downcast_ref::<&str>()
		.copied()
		.unwrap_or_default()
}

#[inline(always)]
#[must_use]
pub fn rttype_name<T: ?Sized>(_: &T) -> &'static str { type_name::<T>() }

#[inline(always)]
#[must_use]
pub fn type_name<T: ?Sized>() -> &'static str { std::any::type_name::<T>() }

/// Returns true if debug logging is enabled. In this mode extra logging calls
/// are made at all log levels, not just DEBUG and TRACE. These logs are demoted
/// to DEBUG level when this function returns false; as a consequence they will
/// be elided by `release_max_log_level` when featured.
#[must_use]
#[inline]
pub const fn logging() -> bool {
	cfg!(debug_assertions)
		|| cfg!(tuwunel_debug_logging)
		|| !cfg!(feature = "release_max_log_level")
}
