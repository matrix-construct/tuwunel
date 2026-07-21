use std::sync::atomic::{AtomicUsize, Ordering};

/// Stream concurrency factor; this is a live value.
static WIDTH: AtomicUsize = AtomicUsize::new(32);

/// Stream throughput amplifier; this is a live value.
static AMPLIFICATION: AtomicUsize = AtomicUsize::new(1024);

/// Practicable limits on the stream width.
pub const WIDTH_LIMIT: (usize, usize) = (1, 1024);

/// Practicable limits on the stream amplifier.
pub const AMPLIFICATION_LIMIT: (usize, usize) = (32, 32768);

/// Sets the live concurrency factor. The first return value is the previous
/// width which was replaced. The second return value is the value which was set
/// after any applied limits.
pub fn set_width(width: usize) -> (usize, usize) {
	let width = width.clamp(WIDTH_LIMIT.0, WIDTH_LIMIT.1);
	(WIDTH.swap(width, Ordering::Relaxed), width)
}

/// Sets the live concurrency amplification. The first return value is the
/// previous width which was replaced. The second return value is the value
/// which was set after any applied limits.
pub fn set_amplification(width: usize) -> (usize, usize) {
	let width = width.clamp(AMPLIFICATION_LIMIT.0, AMPLIFICATION_LIMIT.1);
	(AMPLIFICATION.swap(width, Ordering::Relaxed), width)
}

/// Returns the live default concurrency width for stream operations.
///
/// Startup tuning selects a value for the host, and later updates take effect
/// immediately. Operations with an explicit width bypass this default.
#[inline]
pub fn automatic_width() -> usize {
	let width = WIDTH.load(Ordering::Relaxed);
	debug_assert!(width >= WIDTH_LIMIT.0, "WIDTH should not be zero");
	debug_assert!(width <= WIDTH_LIMIT.1, "WIDTH is probably too large");
	width
}

/// Used by stream operations where the amplification hasn't been manually
/// supplied by the caller. Instead we provide a computed value.
#[inline]
pub fn automatic_amplification() -> usize {
	let amplification = AMPLIFICATION.load(Ordering::Relaxed);
	debug_assert!(amplification >= AMPLIFICATION_LIMIT.0, "amplification is too low");
	debug_assert!(amplification <= AMPLIFICATION_LIMIT.1, "amplification is too high");
	amplification
}
