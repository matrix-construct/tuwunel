//! Arithmetic checking and numeric conversion helpers.
//!
//! Exported macros separate recoverable, expected, and prevalidated arithmetic.
//! Conversion helpers centralize errors, panics, and deliberate truncation.

use std::num::NonZeroUsize;

mod expect_into;
mod expected;
#[cfg(test)]
mod tests;
mod tried;

/// Transforms arithmetic expressions into checked operations.
///
/// Successful evaluation yields [`Some`], while a failed operation yields
/// [`None`]. The [`crate::checked!`] macro converts that optional result into
/// crate error handling.
pub use checked_ops::checked_ops;

/// Converts values with [`TryFrom`] and panics on failure.
///
/// The conversion delegates to [`expect_into`] and panics on failure. Its
/// destination type can be inferred from the call context.
pub use self::expect_into::ExpectInto;
/// Adds checked arithmetic methods that panic on failure.
///
/// Each operation panics when its underlying checked operation fails. The trait
/// covers addition, subtraction, multiplication, division, and remainder.
pub use self::expected::Expected;
/// Adds checked arithmetic methods that return a [`Result`].
///
/// Each operation returns [`Error::Arithmetic`] when its checked operation
/// fails. The trait covers addition, subtraction, multiplication, division, and
/// remainder.
pub use self::tried::Tried;
use crate::{Err, Error, Result, debug::type_name, err};

#[expect(
	clippy::lossy_float_literal,
	reason = "2^64 is exactly representable"
)]
const USIZE_MAX_EXCLUSIVE: f64 = match usize::BITS {
	| 16 => 65_536.0,
	| 32 => 4_294_967_296.0,
	| 64 => 18_446_744_073_709_551_616.0,
	| _ => panic!("unsupported usize width"),
};

/// Combines per-call and configuration concurrency caps.
///
/// An absent per-call cap and a zero configuration cap each mean unbounded.
/// When both sides are bounded, the lower cap wins.
#[inline]
#[must_use]
pub fn effective_cap(requested: Option<NonZeroUsize>, config: usize) -> usize {
	requested
		.map_or(usize::MAX, NonZeroUsize::get)
		.min(NonZeroUsize::new(config).map_or(usize::MAX, NonZeroUsize::get))
}

/// Evaluates a checked arithmetic expression as a [`Result`].
///
/// A successful expression returns its value. Overflow or another invalid
/// operation returns [`Error::Arithmetic`] through a cold error path.
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! checked {
	($($input:tt)+) => {
		$crate::utils::math::checked_ops!($($input)+)
			.ok_or_else(
				// The compiler will now attempt to inline the math predicate
				// while moving the error handling out to .text.unlikely.
				#[cold]
				|| $crate::err!(Arithmetic("operation overflowed or result invalid"))
			)
	};
}

/// Evaluates a checked arithmetic expression and panics on failure.
///
/// Use this when failure is not realistically expected but the expression does
/// not meet the safety bar for `validated!`. The first form accepts a custom
/// panic message; the second uses a default.
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! expected {
	($msg:literal, $($input:tt)+) => {
		$crate::checked!($($input)+).expect($msg)
	};

	($($input:tt)+) => {
		$crate::expected!("arithmetic expression expectation failure", $($input)+)
	};
}

/// Evaluates arithmetic with checks enabled only in debug builds.
///
/// Debug builds use checked operations and panic when the expression overflows
/// or is otherwise invalid. Release builds evaluate the expression directly,
/// so callers must ensure every operation is valid.
#[cfg(not(debug_assertions))]
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! validated {
	($($input:tt)+) => {
		{
			// TODO rewrite when stmt_expr_attributes is stable
			#[expect(clippy::arithmetic_side_effects)]
			let __res = ($($input)+);
			__res
		}
	};
}

/// Evaluates arithmetic with checks enabled only in debug builds.
///
/// Debug builds use checked operations and panic when the expression overflows
/// or is otherwise invalid. Release builds evaluate the expression directly,
/// so callers must ensure every operation is valid.
#[cfg(debug_assertions)]
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! validated {
	($($input:tt)+) => {
		$crate::expected!("validated arithmetic expression failed", $($input)+)
	}
}

/// Converts a representable nonnegative `f64` to `usize` by truncating toward
/// zero.
///
/// Negative, non-finite, and out-of-range values return [`Error::Arithmetic`].
/// Negative zero is accepted; valid fractional values are truncated toward
/// zero.
#[inline]
pub fn usize_from_f64(val: f64) -> Result<usize, Error> {
	if !(0.0..USIZE_MAX_EXCLUSIVE).contains(&val) {
		return Err!(Arithmetic("Float is not representable as usize"));
	}

	// SAFETY: The range check proves `val` is finite, nonnegative, and
	// representable after truncation.
	Ok(unsafe { val.to_int_unchecked::<usize>() })
}

/// Converts a Matrix unsigned integer to `usize`.
///
/// The conversion is exact. It panics if the value exceeds the platform's
/// `usize` range.
#[inline]
#[must_use]
pub fn usize_from_ruma(val: ruma::UInt) -> usize {
	usize::try_from(val).expect("failed conversion from ruma::UInt to usize")
}

/// Converts a `u64` to a Matrix unsigned integer.
///
/// The conversion is exact. It panics if the value exceeds the range supported
/// by [`ruma::UInt`].
#[inline]
#[must_use]
pub fn ruma_from_u64(val: u64) -> ruma::UInt {
	ruma::UInt::try_from(val).expect("failed conversion from u64 to ruma::UInt")
}

/// Converts a `usize` to a Matrix unsigned integer.
///
/// The conversion is exact. It panics if the value exceeds the range supported
/// by [`ruma::UInt`].
#[inline]
#[must_use]
pub fn ruma_from_usize(val: usize) -> ruma::UInt {
	ruma::UInt::try_from(val).expect("failed conversion from usize to ruma::UInt")
}

/// Converts a `u64` to `usize` with deliberate truncation when necessary.
///
/// Targets with a narrower `usize` discard the high bits. The conversion is
/// exact when `usize` is at least 64 bits wide.
#[inline]
#[must_use]
#[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
pub fn usize_from_u64_truncated(val: u64) -> usize { val as usize }

/// Converts a value with [`TryFrom`] and panics if conversion fails.
///
/// Successful conversions return the destination value. A failed conversion
/// terminates with a fixed expectation message.
#[inline]
pub fn expect_into<Dst: TryFrom<Src>, Src>(src: Src) -> Dst {
	try_into(src).expect("failed conversion from Src to Dst")
}

/// Converts a value with [`TryFrom`] and maps failure to an arithmetic error.
///
/// Successful conversions return the destination value unchanged. A failure
/// records the source and destination type names and discards the original
/// error.
#[inline]
pub fn try_into<Dst: TryFrom<Src>, Src>(src: Src) -> Result<Dst> {
	Dst::try_from(src).map_err(|_| {
		err!(Arithmetic(
			"failed to convert from {} to {}",
			type_name::<Src>(),
			type_name::<Dst>()
		))
	})
}
