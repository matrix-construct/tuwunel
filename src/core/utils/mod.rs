//! Reusable helpers and extension traits for core services.
//!
//! The module groups compile-time assertions, closure-producing macros, and
//! common utilities shared across workspace crates. Frequently used helpers are
//! re-exported through a single import path.

/// Extensions for fixed-capacity `ArrayVec` values.
///
/// The module adds fluent slice extension while preserving the collection's
/// fixed storage budget. Capacity exhaustion remains explicit through a panic.
pub mod arrayvec;
pub mod bool;
/// Byte-size parsing, display, and integer encoding helpers.
///
/// The module handles human-readable sizes alongside fixed-width big-endian
/// counters. Its size deserializers integrate human-readable values with Serde.
pub mod bytes;
/// HTTP content-disposition selection and filename sanitization.
///
/// Media types are checked against the safe-inline list before selecting a
/// disposition. Optional filenames are sanitized before header construction.
pub mod content_disposition;
pub mod debug;
pub mod defer;
/// Additional combinators for futures and optional futures.
///
/// The extensions compose Boolean, optional, and fallible outputs without
/// awaiting them early. Comparison and selection helpers are included.
pub mod future;
pub mod hash;
pub mod html;
pub mod json;
pub mod math;
pub mod mutex_map;
pub mod option;
pub mod rand;
/// Result aliases and composable result extensions.
///
/// The module covers filtering, inspection, logging, flattening, and
/// expectation helpers. Most adapters preserve the original result type.
pub mod result;
/// Configuration-secret storage and resolution helpers.
///
/// Secrets may come from files or inline configuration. The storage type does
/// not redact output or erase memory when dropped.
pub mod secret;
pub mod set;
/// Stream adapters for synchronous, concurrent, and fallible transformations.
///
/// The module extends futures streams with ordered and completion-ordered
/// concurrency, ready closures, aggregation, and result handling.
pub mod stream;
/// String conversion, serialization, and formatting helpers.
///
/// The module includes borrowed unquoted views, serde adapters, chunking, and
/// UTF-8 conversion utilities. Specialized display wrappers avoid allocation.
pub mod string;
pub mod sys;
#[cfg(test)]
mod tests;
pub mod time;
pub mod two_phase_counter;
pub mod unhandled;

pub use ::ctor::ctor;
pub use ::dtor::dtor;
pub use ::tuwunel_macros::{async_noinline, implement};

pub use self::{
	arrayvec::ArrayVecExt,
	bool::BoolExt,
	bytes::{increment, u64_from_bytes, u64_from_u8},
	debug::slice_truncated as debug_slice_truncated,
	future::{BoolExt as FutureBoolExt, OptionStream, TryExtExt as TryFutureExtExt},
	hash::sha256::delimited as calculate_hash,
	json::{deserialize_from_str, to_canonical_object},
	mutex_map::{Guard as MutexMapGuard, MutexMap},
	option::OptionExt,
	rand::{shuffle, string as random_string, string_from as random_string_from},
	secret::{Secret, is_set as is_secret_set, resolve as resolve_secret},
	stream::{IterStream, ReadyExt, Tools as StreamTools, TryReadyExt},
	string::{str_from_bytes, string_from_bytes},
	sys::compute::available_parallelism,
	time::{
		exponential_backoff::{
			continue_exponential_backoff, continue_exponential_backoff_secs,
			exponential_backoff_streak_cap,
		},
		now_millis as millis_since_unix_epoch, timepoint_ago, timepoint_from_now,
		timepoint_has_passed,
	},
};

/// Asserts at compile time that `T` implements `Send`.
///
/// The generic bound supplies the assertion, and the function performs no
/// runtime work.
pub const fn assert_send<T: Send>() {}

/// Asserts at compile time that `T` implements `Sync`.
///
/// The generic bound supplies the assertion, and the function performs no
/// runtime work.
pub const fn assert_sync<T: Sync>() {}

/// Accepts any type, including an unsized type, without performing work.
///
/// The `?Sized` bound removes the implicit `Sized` requirement. The const
/// signature permits use from const contexts.
pub const fn assert_dst<T: ?Sized>() {}

/// Asserts at compile time that `T` implements `Sized`.
///
/// The generic bound supplies the assertion, and the function performs no
/// runtime work.
pub const fn assert_sized<T: Sized>() {}

/// Asserts at compile time that `T` implements `Unpin`.
///
/// The generic bound supplies the assertion, and the function performs no
/// runtime work.
pub const fn assert_unpin<T: Unpin>() {}

/// Asserts at compile time that `T` implements `UnwindSafe`.
///
/// The generic bound supplies the assertion, and the function performs no
/// runtime work.
pub const fn assert_unwind_safe<T: std::panic::UnwindSafe>() {}

/// Asserts at compile time that `T` implements `RefUnwindSafe`.
///
/// The generic bound supplies the assertion, and the function performs no
/// runtime work.
pub const fn assert_ref_unwind_safe<T: std::panic::RefUnwindSafe>() {}

/// Extracts the payload from any listed tuple variant into an `Option`.
///
/// The expression is matched once, and a listed variant returns
/// `Some(payload)`. Every other variant returns `None`.
#[macro_export]
macro_rules! extract_variant {
	( $e:expr_2021, $( $variant:path )|* ) => {
		match $e {
			$( $variant(value) => Some(value), )*
			_ => None,
		}
	};
}

/// Extracts a pattern-bound value into an `Option`.
///
/// The expression is matched once against the supplied pattern. A match returns
/// the named binding in `Some`, while every other value returns `None`.
#[macro_export]
macro_rules! extract {
	($e:expr_2021, $out:ident in $variant:pat) => {
		match $e {
			| $variant => Some($out),
			| _ => None,
		}
	};
}

/// Creates a closure that reports whether its input is nonempty.
///
/// The generated closure delegates to `is_empty` and negates the result.
#[macro_export]
macro_rules! is_not_empty {
	() => {
		|x| !x.is_empty()
	};
}

/// Creates a closure that applies one callable to every field of a tuple.
///
/// Tuple arities from one through five are supported. The callable tokens are
/// expanded once for each field and therefore may be evaluated more than once.
#[macro_export]
macro_rules! apply {
	(1, $($idx:tt)+) => {
		|t| (($($idx)+)(t.0),)
	};

	(2, $($idx:tt)+) => {
		|t| (($($idx)+)(t.0), ($($idx)+)(t.1),)
	};

	(3, $($idx:tt)+) => {
		|t| (($($idx)+)(t.0), ($($idx)+)(t.1), ($($idx)+)(t.2),)
	};

	(4, $($idx:tt)+) => {
		|t| (($($idx)+)(t.0), ($($idx)+)(t.1), ($($idx)+)(t.2), ($($idx)+)(t.3),)
	};

	(5, $($idx:tt)+) => {
		|t| (($($idx)+)(t.0), ($($idx)+)(t.1), ($($idx)+)(t.2), ($($idx)+)(t.3), ($($idx)+)(t.4),)
	};
}

/// Expands a type or expression into a two-element tuple with identical
/// entries.
///
/// The type form produces `(T, T)`. The expression form evaluates the supplied
/// expression separately for each tuple element.
#[macro_export]
macro_rules! pair_of {
	($decl:ty) => {
		($decl, $decl)
	};

	($init:expr_2021) => {
		($init, $init)
	};
}

/// Creates a Boolean identity closure.
///
/// The generated closure applies logical negation twice, making it usable where
/// a predicate function is required.
#[macro_export]
macro_rules! is_true {
	() => {
		|x| !!x
	};
}

/// Creates a closure that negates a Boolean input.
///
/// The generated closure returns `!x` and can be passed directly to predicate
/// combinators.
#[macro_export]
macro_rules! is_false {
	() => {
		|x| !x
	};
}

/// Creates a closure that reports whether its input differs from zero.
///
/// The generated closure compares each input with the integer literal `0`.
#[macro_export]
macro_rules! is_nonzero {
	() => {
		|x| x != 0
	};
}

/// Creates a closure that reports whether its input matches zero.
///
/// The generated closure uses a literal pattern through `is_matching!`.
#[macro_export]
macro_rules! is_zero {
	() => {
		$crate::is_matching!(0)
	};
}

/// Creates a closure that compares each input with a supplied value for
/// equality.
///
/// The comparison target remains inside the closure body and is evaluated on
/// every call.
#[macro_export]
macro_rules! is_equal_to {
	($val:ident) => {
		|x| x == $val
	};

	($val:expr_2021) => {
		|x| x == $val
	};
}

/// Creates a closure that compares each input with a supplied value for
/// inequality.
///
/// The comparison target remains inside the closure body and is evaluated on
/// every call.
#[macro_export]
macro_rules! is_not_equal_to {
	($val:ident) => {
		|x| x != $val
	};

	($val:expr_2021) => {
		|x| x != $val
	};
}

/// Creates a closure that reports whether each input is less than a supplied
/// value.
///
/// The comparison target remains inside the closure body and is evaluated on
/// every call.
#[macro_export]
macro_rules! is_less_than {
	($val:ident) => {
		|x| x < $val
	};

	($val:expr_2021) => {
		|x| x < $val
	};
}

/// Creates a closure that tests its input with a `matches!` pattern.
///
/// The supplied tokens can contain any pattern form accepted by `matches!`. The
/// closure returns false when the input does not match.
#[macro_export]
macro_rules! is_matching {
	($val:ident) => {
		|x| matches!(x, $val)
	};

	($($val:tt)+) => {
		|x| matches!(x, $($val)+)
	};
}

/// Creates a two-argument closure that compares its inputs for equality.
///
/// The generated closure returns the result of `a == b`.
#[macro_export]
macro_rules! is_equal {
	() => {
		|a, b| a == b
	};
}

/// Creates a closure that dereferences an indexed tuple field.
///
/// The tuple argument is received by value, and the selected field is returned
/// through unary dereference.
#[macro_export]
macro_rules! deref_at {
	($idx:tt) => {
		|t| *t.$idx
	};
}

/// Creates a closure that borrows an indexed tuple field.
///
/// The generated `ref` pattern borrows the tuple argument before returning a
/// reference to the selected field.
#[macro_export]
macro_rules! ref_at {
	($idx:tt) => {
		|ref t| &t.$idx
	};
}

/// Creates a closure that returns an indexed field from a referenced tuple by
/// value.
///
/// The generated pattern destructures the shared reference before selecting the
/// field. Moving the tuple from that reference therefore requires a copyable
/// value.
#[macro_export]
macro_rules! val_at {
	($idx:tt) => {
		|&t| t.$idx
	};
}

/// Creates a closure that selects an indexed tuple field by value.
///
/// The generated closure consumes its tuple argument and returns the selected
/// field.
#[macro_export]
macro_rules! at {
	($idx:tt) => {
		|t| t.$idx
	};
}
