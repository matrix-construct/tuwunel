#![expect(
	clippy::cast_possible_wrap,
	clippy::cast_sign_loss,
	clippy::as_conversions
)]

use std::{cmp::Ordering, fmt, fmt::Display, str::FromStr};

use ruma::api::Direction;

use crate::{Error, Result, err};

/// Sequence number locating a PDU in a room timeline.
///
/// Valid normal counts range from zero through `i64::MAX`, while valid
/// backfilled counts range from `i64::MIN` through zero. Ordering compares
/// their signed representations, with zero shared by both variants.
#[derive(Hash, PartialEq, Eq, Clone, Copy, Debug)]
pub enum Count {
	/// Sequence assigned to an event in the normal timeline.
	///
	/// Normal counts advance forward as new local or federated events are
	/// appended. Valid values do not exceed `i64::MAX`.
	Normal(u64),

	/// Sequence assigned to an event in the backfilled timeline.
	///
	/// Valid backfilled counts occupy the nonpositive signed ordering range.
	Backfilled(i64),
}

impl Count {
	/// Encodes the count's integer bits in big-endian order.
	///
	/// Normal values retain their unsigned representation. Backfilled values
	/// are reinterpreted as unsigned two's-complement bits before encoding.
	///
	/// # Panics
	///
	/// Panics in debug builds if a positive `Backfilled` value was constructed
	/// directly.
	#[inline]
	#[must_use]
	pub fn to_be_bytes(self) -> [u8; size_of::<u64>()] { self.into_unsigned().to_be_bytes() }

	/// Interprets unsigned integer bits as a signed timeline count.
	///
	/// Values with the high bit set become negative backfilled counts. Other
	/// positive values become normal counts, while zero becomes backfilled
	/// zero.
	#[inline]
	#[must_use]
	pub fn from_unsigned(unsigned: u64) -> Self { Self::from_signed(unsigned as i64) }

	/// Classifies a signed integer as a normal or backfilled count.
	///
	/// Positive values become normal counts. Zero and negative values become
	/// backfilled counts.
	#[inline]
	#[must_use]
	pub fn from_signed(signed: i64) -> Self {
		match signed {
			| i64::MIN..=0 => Self::Backfilled(signed),
			| _ => Self::Normal(signed as u64),
		}
	}

	/// Converts the count to its unsigned integer bit representation.
	///
	/// Backfilled values are reinterpreted using two's-complement bits. The
	/// variant information is not retained in the returned integer.
	///
	/// # Panics
	///
	/// Panics in debug builds if a positive `Backfilled` value was constructed
	/// directly.
	#[inline]
	#[must_use]
	pub fn into_unsigned(self) -> u64 {
		self.debug_assert_valid();
		match self {
			| Self::Normal(i) => i,
			| Self::Backfilled(i) => i as u64,
		}
	}

	/// Converts the count to the signed representation used for ordering.
	///
	/// Valid normal values cast into the nonnegative signed domain, while valid
	/// backfilled values retain their signed count. Direct construction outside
	/// the documented ranges violates the ordering invariant.
	///
	/// # Panics
	///
	/// Panics in debug builds if a positive `Backfilled` value was constructed
	/// directly.
	#[inline]
	#[must_use]
	pub fn into_signed(self) -> i64 {
		self.debug_assert_valid();
		match self {
			| Self::Normal(i) => i as i64,
			| Self::Backfilled(i) => i,
		}
	}

	/// Converts a count to a normal-timeline position.
	///
	/// Existing normal counts are preserved. Any backfilled count maps to the
	/// beginning of the normal timeline at zero.
	///
	/// # Panics
	///
	/// Panics in debug builds if a positive `Backfilled` value was constructed
	/// directly.
	#[inline]
	#[must_use]
	pub fn into_normal(self) -> Self {
		self.debug_assert_valid();
		match self {
			| Self::Normal(i) => Self::Normal(i),
			| Self::Backfilled(_) => Self::Normal(0),
		}
	}

	/// Advances or retreats the count by one without integer overflow.
	///
	/// Forward movement adds one and backward movement subtracts one. The
	/// original variant is preserved, so callers must avoid crossing that
	/// variant's valid timeline range.
	#[inline]
	pub fn checked_inc(self, dir: Direction) -> Result<Self, Error> {
		match dir {
			| Direction::Forward => self.checked_add(1),
			| Direction::Backward => self.checked_sub(1),
		}
	}

	/// Adds an unsigned offset without integer overflow.
	///
	/// Backfilled offsets are cast to `i64` and must not exceed `i64::MAX`.
	/// Callers must keep the resulting variant within its documented range;
	/// integer overflow returns an arithmetic error.
	#[inline]
	pub fn checked_add(self, add: u64) -> Result<Self, Error> {
		Ok(match self {
			| Self::Normal(i) => Self::Normal(
				i.checked_add(add)
					.ok_or_else(|| err!(Arithmetic("Count::Normal overflow")))?,
			),
			| Self::Backfilled(i) => Self::Backfilled(
				i.checked_add(add as i64)
					.ok_or_else(|| err!(Arithmetic("Count::Backfilled overflow")))?,
			),
		})
	}

	/// Subtracts an unsigned offset without integer underflow.
	///
	/// Backfilled offsets are cast to `i64` and must not exceed `i64::MAX`.
	/// Callers must keep the resulting variant within its documented range;
	/// integer underflow returns an arithmetic error.
	#[inline]
	pub fn checked_sub(self, sub: u64) -> Result<Self, Error> {
		Ok(match self {
			| Self::Normal(i) => Self::Normal(
				i.checked_sub(sub)
					.ok_or_else(|| err!(Arithmetic("Count::Normal underflow")))?,
			),
			| Self::Backfilled(i) => Self::Backfilled(
				i.checked_sub(sub as i64)
					.ok_or_else(|| err!(Arithmetic("Count::Backfilled underflow")))?,
			),
		})
	}

	/// Advances or retreats the count by one with saturation.
	///
	/// Forward movement adds one and backward movement subtracts one.
	/// Saturation uses the integer boundary of the existing variant and does
	/// not prevent a backfilled value from crossing zero.
	#[inline]
	#[must_use]
	pub fn saturating_inc(self, dir: Direction) -> Self {
		match dir {
			| Direction::Forward => self.saturating_add(1),
			| Direction::Backward => self.saturating_sub(1),
		}
	}

	/// Adds an unsigned offset with saturation at the integer boundary.
	///
	/// Backfilled offsets are cast to `i64` and must not exceed `i64::MAX`.
	/// Saturation uses the underlying integer boundary and does not repair a
	/// result outside the variant's documented range.
	#[inline]
	#[must_use]
	pub fn saturating_add(self, add: u64) -> Self {
		match self {
			| Self::Normal(i) => Self::Normal(i.saturating_add(add)),
			| Self::Backfilled(i) => Self::Backfilled(i.saturating_add(add as i64)),
		}
	}

	/// Subtracts an unsigned offset with saturation at the integer boundary.
	///
	/// Backfilled offsets are cast to `i64` and must not exceed `i64::MAX`.
	/// Saturation uses the underlying integer boundary and does not repair a
	/// result outside the variant's documented range.
	#[inline]
	#[must_use]
	pub fn saturating_sub(self, sub: u64) -> Self {
		match self {
			| Self::Normal(i) => Self::Normal(i.saturating_sub(sub)),
			| Self::Backfilled(i) => Self::Backfilled(i.saturating_sub(sub as i64)),
		}
	}

	/// Returns the earliest valid timeline count.
	///
	/// The minimum is a backfilled count at `i64::MIN`. It sorts before every
	/// other valid count.
	#[inline]
	#[must_use]
	pub const fn min() -> Self { Self::Backfilled(i64::MIN) }

	/// Returns the latest valid timeline count.
	///
	/// The maximum is a normal count at `i64::MAX`. This keeps the value within
	/// the signed domain used by ordering.
	#[inline]
	#[must_use]
	pub const fn max() -> Self { Self::Normal(i64::MAX as u64) }

	#[inline]
	pub(crate) fn debug_assert_valid(&self) {
		if let Self::Backfilled(i) = self {
			debug_assert!(*i <= 0, "Backfilled sequence must be negative");
		}
	}
}

impl Display for Count {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
		self.debug_assert_valid();
		match self {
			| Self::Normal(i) => write!(f, "{i}"),
			| Self::Backfilled(i) => write!(f, "{i}"),
		}
	}
}

impl From<i64> for Count {
	#[inline]
	fn from(signed: i64) -> Self { Self::from_signed(signed) }
}

impl From<u64> for Count {
	#[inline]
	fn from(unsigned: u64) -> Self { Self::from_unsigned(unsigned) }
}

impl FromStr for Count {
	type Err = Error;

	fn from_str(token: &str) -> Result<Self, Self::Err> { Ok(Self::from_signed(token.parse()?)) }
}

impl PartialOrd for Count {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl Ord for Count {
	fn cmp(&self, other: &Self) -> Ordering { self.into_signed().cmp(&other.into_signed()) }
}

impl Default for Count {
	fn default() -> Self { Self::Normal(0) }
}
