//! Retry-backoff calculations.
//!
//! The helpers determine whether a retry delay remains active and derive
//! retry-streak caps from duration bounds. Delay calculations saturate at the
//! configured maximum where applicable.

use std::time::Duration;

/// Returns false if the exponential backoff has expired based on the inputs
#[inline]
#[must_use]
pub fn continue_exponential_backoff_secs(
	min: u64,
	max: u64,
	elapsed: Duration,
	tries: u32,
) -> bool {
	exponential_backoff_remaining_secs(min, max, elapsed, tries).is_some()
}

/// Returns false if the exponential backoff has expired based on the inputs
#[inline]
#[must_use]
pub fn continue_exponential_backoff(
	min: Duration,
	max: Duration,
	elapsed: Duration,
	tries: u32,
) -> bool {
	exponential_backoff_remaining(min, max, elapsed, tries).is_some()
}

/// Calculates the remaining exponential-backoff hold from whole-second bounds.
///
/// The bounds are converted to durations before applying the quadratic retry
/// curve. `None` means the hold has elapsed.
#[inline]
#[must_use]
pub fn exponential_backoff_remaining_secs(
	min: u64,
	max: u64,
	elapsed: Duration,
	tries: u32,
) -> Option<Duration> {
	let min = Duration::from_secs(min);
	let max = Duration::from_secs(max);

	exponential_backoff_remaining(min, max, elapsed, tries)
}

/// Calculates the remaining exponential-backoff hold.
///
/// The window grows quadratically with the retry count and is capped at
/// `max`. `None` means the hold has elapsed.
#[inline]
#[must_use]
pub fn exponential_backoff_remaining(
	min: Duration,
	max: Duration,
	elapsed: Duration,
	tries: u32,
) -> Option<Duration> {
	let window = min
		.saturating_mul(tries)
		.saturating_mul(tries)
		.min(max);

	window
		.checked_sub(elapsed)
		.filter(|remaining| !remaining.is_zero())
}

/// Derives a retry-streak cap from the whole-second ratio of `max` to `min`.
///
/// Let `r = max.as_secs() / min.as_secs().max(1)` using integer division. The
/// result is `ceil(sqrt(r))`, clamped to the range `1..=u32::MAX`. Subsecond
/// components and the division remainder are discarded.
#[inline]
#[must_use]
pub fn exponential_backoff_streak_cap(min: Duration, max: Duration) -> u32 {
	let min_secs = min.as_secs().max(1);
	let ratio = max.as_secs().checked_div(min_secs).unwrap_or(0);
	let floor = ratio.isqrt();
	let ceil = if floor.saturating_mul(floor) < ratio {
		floor.saturating_add(1)
	} else {
		floor
	};

	u32::try_from(ceil).unwrap_or(u32::MAX).max(1)
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::{exponential_backoff_remaining, exponential_backoff_remaining_secs};

	#[test]
	fn remaining_tracks_the_quadratic_window() {
		let min = Duration::from_secs(3);
		let max = Duration::from_mins(1);
		let elapsed = Duration::from_secs(5);

		assert_eq!(
			exponential_backoff_remaining(min, max, elapsed, 2),
			Some(Duration::from_secs(7)),
		);
	}

	#[test]
	fn remaining_caps_and_expires_at_the_boundary() {
		let min = Duration::from_secs(3);
		let max = Duration::from_mins(1);
		let elapsed = Duration::from_mins(1);

		assert_eq!(
			exponential_backoff_remaining(min, max, Duration::from_secs(15), 100),
			Some(Duration::from_secs(45)),
		);

		assert_eq!(exponential_backoff_remaining(min, max, elapsed, 100), None);
		assert_eq!(
			exponential_backoff_remaining(Duration::ZERO, max, Duration::ZERO, u32::MAX),
			None,
		);
		assert_eq!(
			exponential_backoff_remaining(
				Duration::MAX,
				Duration::MAX,
				Duration::from_secs(1),
				u32::MAX,
			),
			Duration::MAX.checked_sub(Duration::from_secs(1)),
		);
	}

	#[test]
	fn seconds_wrapper_matches_duration_bounds() {
		let elapsed = Duration::from_secs(5);

		assert_eq!(
			exponential_backoff_remaining_secs(3, 60, elapsed, 2),
			exponential_backoff_remaining(
				Duration::from_secs(3),
				Duration::from_mins(1),
				elapsed,
				2,
			),
		);
	}
}
