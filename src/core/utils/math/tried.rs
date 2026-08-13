use num_traits::ops::checked::{CheckedAdd, CheckedDiv, CheckedMul, CheckedRem, CheckedSub};

use crate::{Result, checked};

/// Provides checked arithmetic that reports overflow and invalid operations.
///
/// Each method delegates to the corresponding `Checked*` trait. A successful
/// operation returns its value through the crate's result type.
pub trait Tried {
	/// Adds `rhs` with checked arithmetic.
	///
	/// Values that the underlying [`CheckedAdd`] implementation accepts are
	/// returned unchanged. Overflow is represented by the crate's arithmetic
	/// error.
	#[inline]
	fn try_add(self, rhs: Self) -> Result<Self>
	where
		Self: CheckedAdd + Sized,
	{
		checked!(self + rhs)
	}

	/// Subtracts `rhs` with checked arithmetic.
	///
	/// Values that the underlying [`CheckedSub`] implementation accepts are
	/// returned unchanged. Overflow is represented by the crate's arithmetic
	/// error.
	#[inline]
	fn try_sub(self, rhs: Self) -> Result<Self>
	where
		Self: CheckedSub + Sized,
	{
		checked!(self - rhs)
	}

	/// Multiplies by `rhs` with checked arithmetic.
	///
	/// Values that the underlying [`CheckedMul`] implementation accepts are
	/// returned unchanged. Overflow is represented by the crate's arithmetic
	/// error.
	#[inline]
	fn try_mul(self, rhs: Self) -> Result<Self>
	where
		Self: CheckedMul + Sized,
	{
		checked!(self * rhs)
	}

	/// Divides by `rhs` with checked arithmetic.
	///
	/// Values that the underlying [`CheckedDiv`] implementation accepts are
	/// returned unchanged. Division by zero or overflow becomes the crate's
	/// arithmetic error.
	#[inline]
	fn try_div(self, rhs: Self) -> Result<Self>
	where
		Self: CheckedDiv + Sized,
	{
		checked!(self / rhs)
	}

	/// Computes the remainder by `rhs` with checked arithmetic.
	///
	/// Values that the underlying [`CheckedRem`] implementation accepts are
	/// returned unchanged. A failed checked remainder, including a zero divisor
	/// or overflow, becomes the crate's arithmetic error.
	#[inline]
	fn try_rem(self, rhs: Self) -> Result<Self>
	where
		Self: CheckedRem + Sized,
	{
		checked!(self % rhs)
	}
}

impl<T> Tried for T {}
