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
	let min = Duration::from_secs(min);
	let max = Duration::from_secs(max);
	continue_exponential_backoff(min, max, elapsed, tries)
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
	let min = min
		.saturating_mul(tries)
		.saturating_mul(tries)
		.min(max);

	elapsed < min
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
