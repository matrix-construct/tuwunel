pub mod arrayvec;
pub mod bool;
pub mod bytes;
pub mod content_disposition;
pub mod debug;
pub mod defer;
pub mod future;
pub mod hash;
pub mod json;
pub mod math;
pub mod mutex_map;
pub mod option;
pub mod rand;
pub mod result;
pub mod set;
pub mod stream;
pub mod string;
pub mod sys;
#[cfg(test)]
mod tests;
pub mod time;
pub mod two_phase_counter;

pub use ::ctor::{ctor, dtor};
pub use ::tuwunel_macros::implement;

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
	rand::{shuffle, string as random_string},
	stream::{IterStream, ReadyExt, Tools as StreamTools, TryReadyExt},
	string::{str_from_bytes, string_from_bytes},
	sys::compute::available_parallelism,
	time::{
		exponential_backoff::{continue_exponential_backoff, continue_exponential_backoff_secs},
		now_millis as millis_since_unix_epoch, timepoint_ago, timepoint_from_now,
		timepoint_has_passed,
	},
};

pub const fn assert_send<T: Send>() {}
pub const fn assert_sync<T: Sync>() {}
pub const fn assert_dst<T: ?Sized>() {}
pub const fn assert_sized<T: Sized>() {}
pub const fn assert_unpin<T: Unpin>() {}
pub const fn assert_unwind_safe<T: std::panic::UnwindSafe>() {}
pub const fn assert_ref_unwind_safe<T: std::panic::RefUnwindSafe>() {}

#[macro_export]
macro_rules! extract_variant {
	( $e:expr_2021, $( $variant:path )|* ) => {
		match $e {
			$( $variant(value) => Some(value), )*
			_ => None,
		}
	};
}

#[macro_export]
macro_rules! extract {
	($e:expr_2021, $out:ident in $variant:pat) => {
		match $e {
			| $variant => Some($out),
			| _ => None,
		}
	};
}

/// Functor for !is_empty()
#[macro_export]
macro_rules! is_not_empty {
	() => {
		|x| !x.is_empty()
	};
}

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

#[macro_export]
macro_rules! pair_of {
	($decl:ty) => {
		($decl, $decl)
	};

	($init:expr_2021) => {
		($init, $init)
	};
}

/// Functor for truthy
#[macro_export]
macro_rules! is_true {
	() => {
		|x| !!x
	};
}

/// Functor for falsy
#[macro_export]
macro_rules! is_false {
	() => {
		|x| !x
	};
}

/// Functor for equality to non-zero
#[macro_export]
macro_rules! is_nonzero {
	() => {
		|x| x != 0
	};
}

/// Functor for equality to zero
#[macro_export]
macro_rules! is_zero {
	() => {
		$crate::is_matching!(0)
	};
}

/// Functor for equality i.e. .is_some_and(is_equal_to!(2))
#[macro_export]
macro_rules! is_equal_to {
	($val:ident) => {
		|x| x == $val
	};

	($val:expr_2021) => {
		|x| x == $val
	};
}

/// Functor for inequality i.e. .is_some_and(is_not_equal_to!(2))
#[macro_export]
macro_rules! is_not_equal_to {
	($val:ident) => {
		|x| x != $val
	};

	($val:expr_2021) => {
		|x| x != $val
	};
}

/// Functor for less i.e. .is_some_and(is_less_than!(2))
#[macro_export]
macro_rules! is_less_than {
	($val:ident) => {
		|x| x < $val
	};

	($val:expr_2021) => {
		|x| x < $val
	};
}

/// Functor for matches! i.e. .is_some_and(is_matching!('A'..='Z'))
#[macro_export]
macro_rules! is_matching {
	($val:ident) => {
		|x| matches!(x, $val)
	};

	($($val:tt)+) => {
		|x| matches!(x, $($val)+)
	};
}

/// Functor for equality i.e. (a, b).map(is_equal!())
#[macro_export]
macro_rules! is_equal {
	() => {
		|a, b| a == b
	};
}

/// Functor for |x| *x.$i
#[macro_export]
macro_rules! deref_at {
	($idx:tt) => {
		|t| *t.$idx
	};
}

/// Functor for |ref x| x.$i
#[macro_export]
macro_rules! ref_at {
	($idx:tt) => {
		|ref t| &t.$idx
	};
}

/// Functor for |&x| x.$i
#[macro_export]
macro_rules! val_at {
	($idx:tt) => {
		|&t| t.$idx
	};
}

/// Functor for |x| x.$i
#[macro_export]
macro_rules! at {
	($idx:tt) => {
		|t| t.$idx
	};
}
