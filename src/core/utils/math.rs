mod expected;
mod tried;

use std::convert::TryFrom;

pub use checked_ops::checked_ops;

pub use self::{expected::Expected, tried::Tried};
use crate::{Err, Error, Result, debug::type_name, err};

/// Checked arithmetic expression. Returns a Result<R, Error::Arithmetic>
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! checked {
	($($input:tt)+) => {
		$crate::utils::math::checked_ops!($($input)+)
			.ok_or_else(|| $crate::err!(Arithmetic("operation overflowed or result invalid")))
	};
}

/// Checked arithmetic expression which panics on failure. This is for
/// expressions which do not meet the threshold for validated! but the caller
/// has no realistic expectation for error and no interest in cluttering the
/// callsite with result handling from checked!.
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

/// Unchecked arithmetic expression in release-mode. Use for performance when
/// the expression is obviously safe. The check remains in debug-mode for
/// regression analysis.
#[cfg(not(debug_assertions))]
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! validated {
	($($input:tt)+) => {
		//#[expect(clippy::arithmetic_side_effects)] {
		//Some($($input)*)
		//	.ok_or_else(|| $crate::err!(Arithmetic("this error should never been seen")))
		//}

		//NOTE: remove me when stmt_expr_attributes is stable
		$crate::expected!("validated arithmetic expression failed", $($input)+)
	};
}

/// Checked arithmetic expression in debug-mode. Use for performance when
/// the expression is obviously safe. The check is elided in release-mode.
#[cfg(debug_assertions)]
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! validated {
	($($input:tt)+) => { $crate::expected!($($input)+) }
}

#[inline]
pub fn usize_from_f64(val: f64) -> Result<usize, Error> {
	if val < 0.0 {
		return Err!(Arithmetic("Converting negative float to unsigned integer"));
	}

	//SAFETY: <https://doc.rust-lang.org/std/primitive.f64.html#method.to_int_unchecked>
	Ok(unsafe { val.to_int_unchecked::<usize>() })
}

#[inline]
#[must_use]
pub fn usize_from_ruma(val: ruma::UInt) -> usize {
	usize::try_from(val).expect("failed conversion from ruma::UInt to usize")
}

#[inline]
#[must_use]
pub fn ruma_from_u64(val: u64) -> ruma::UInt {
	ruma::UInt::try_from(val).expect("failed conversion from u64 to ruma::UInt")
}

#[inline]
#[must_use]
pub fn ruma_from_usize(val: usize) -> ruma::UInt {
	ruma::UInt::try_from(val).expect("failed conversion from usize to ruma::UInt")
}

#[inline]
#[must_use]
#[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
pub fn usize_from_u64_truncated(val: u64) -> usize { val as usize }

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
