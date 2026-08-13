use num_traits::ops::checked::{CheckedAdd, CheckedDiv, CheckedMul, CheckedRem, CheckedSub};

use crate::expected;

/// Provides checked arithmetic for operations expected to succeed.
///
/// Each method delegates to the corresponding `Checked*` trait. A failed check
/// is treated as a violated program invariant and panics.
pub trait Expected {
	/// Adds `rhs` with an expectation that the operation is valid.
	///
	/// A successful checked addition returns its value unchanged.
	///
	/// # Panics
	///
	/// Panics when the underlying [`CheckedAdd`] operation returns `None`.
	#[inline]
	#[must_use]
	fn expected_add(self, rhs: Self) -> Self
	where
		Self: CheckedAdd + Sized,
	{
		expected!(self + rhs)
	}

	/// Subtracts `rhs` with an expectation that the operation is valid.
	///
	/// A successful checked subtraction returns its value unchanged.
	///
	/// # Panics
	///
	/// Panics when the underlying [`CheckedSub`] operation returns `None`.
	#[inline]
	#[must_use]
	fn expected_sub(self, rhs: Self) -> Self
	where
		Self: CheckedSub + Sized,
	{
		expected!(self - rhs)
	}

	/// Multiplies by `rhs` with an expectation that the operation is valid.
	///
	/// A successful checked multiplication returns its value unchanged.
	///
	/// # Panics
	///
	/// Panics when the underlying [`CheckedMul`] operation returns `None`.
	#[inline]
	#[must_use]
	fn expected_mul(self, rhs: Self) -> Self
	where
		Self: CheckedMul + Sized,
	{
		expected!(self * rhs)
	}

	/// Divides by `rhs` with an expectation that the operation is valid.
	///
	/// A successful checked division returns its value unchanged.
	///
	/// # Panics
	///
	/// Panics when the underlying [`CheckedDiv`] operation returns `None`.
	#[inline]
	#[must_use]
	fn expected_div(self, rhs: Self) -> Self
	where
		Self: CheckedDiv + Sized,
	{
		expected!(self / rhs)
	}

	/// Computes the remainder with an expectation that the operation is valid.
	///
	/// A successful checked remainder returns its value unchanged.
	///
	/// # Panics
	///
	/// Panics when the underlying [`CheckedRem`] operation returns `None`.
	#[inline]
	#[must_use]
	fn expected_rem(self, rhs: Self) -> Self
	where
		Self: CheckedRem + Sized,
	{
		expected!(self % rhs)
	}
}

impl<T> Expected for T {}
