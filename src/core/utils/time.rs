//! Wall-clock conversion, parsing, and duration-formatting utilities.
//!
//! The helpers convert between `SystemTime` and the Unix epoch, parse
//! human-readable durations, and choose display units. These clocks are not
//! monotonic and can be affected by system-time changes.

mod elapsed;
pub mod exponential_backoff;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use elapsed::Elapsed;

use crate::{Result, err};

/// Returns the current wall-clock time as whole milliseconds since the Unix
/// epoch.
///
/// The submillisecond remainder is discarded, and counts above `u64::MAX`
/// retain only their low 64 bits. The function panics if the current system
/// clock is earlier than the epoch.
#[inline]
#[must_use]
#[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
pub fn now_millis() -> u64 { now().as_millis() as u64 }

/// Returns the current wall-clock time as whole seconds since the Unix epoch.
///
/// The subsecond remainder is discarded. The function panics if the current
/// system clock is earlier than the epoch.
#[inline]
#[must_use]
pub fn now_secs() -> u64 { now().as_secs() }

/// Returns the current wall-clock duration since the Unix epoch.
///
/// The value comes from `SystemTime` and is not monotonic. The function panics
/// if the current system clock is earlier than the epoch.
#[inline]
#[must_use]
pub fn now() -> Duration {
	UNIX_EPOCH
		.elapsed()
		.expect("positive duration after epoch")
}

/// Converts a system time into a nonnegative duration since the Unix epoch.
///
/// Times before the epoch saturate to [`Duration::ZERO`]. Times at or after the
/// epoch preserve their full representable duration.
#[inline]
#[must_use]
pub fn duration_since_epoch(timepoint: SystemTime) -> Duration {
	timepoint
		.duration_since(UNIX_EPOCH)
		.unwrap_or(Duration::ZERO)
}

/// Adds a duration to the Unix epoch using checked arithmetic.
///
/// The resulting system time is returned when representable. An arithmetic
/// error is returned when the duration exceeds the platform's `SystemTime`
/// range.
#[inline]
pub fn timepoint_from_epoch(duration: Duration) -> Result<SystemTime> {
	UNIX_EPOCH
		.checked_add(duration)
		.ok_or_else(|| err!(Arithmetic("Duration {duration:?} from epoch is too large")))
}

/// Adds a duration to the current wall-clock time using checked arithmetic.
///
/// The current time is sampled once for the calculation. An arithmetic error is
/// returned when the result exceeds the platform's `SystemTime` range.
#[inline]
pub fn timepoint_from_now(duration: Duration) -> Result<SystemTime> {
	SystemTime::now()
		.checked_add(duration)
		.ok_or_else(|| err!(Arithmetic("Duration {duration:?} from now is too large")))
}

/// Subtracts a duration from the current wall-clock time using checked
/// arithmetic.
///
/// The current time is sampled once for the calculation. An arithmetic error is
/// returned when the result precedes the platform's `SystemTime` range.
#[inline]
pub fn timepoint_ago(duration: Duration) -> Result<SystemTime> {
	SystemTime::now()
		.checked_sub(duration)
		.ok_or_else(|| err!(Arithmetic("Duration {duration:?} ago is too large")))
}

/// Parses a duration and returns the wall-clock time that far in the past.
///
/// Input syntax is delegated to [`parse_duration`]. Parsing and
/// checked-subtraction errors are propagated.
#[inline]
pub fn parse_timepoint_ago(ago: &str) -> Result<SystemTime> {
	timepoint_ago(parse_duration(ago)?)
}

/// Parses a human-readable duration with the `cyborgtime` parser.
///
/// Successful input is returned as a standard [`Duration`]. Parser failures are
/// wrapped with the original input for context.
#[inline]
pub fn parse_duration(duration: &str) -> Result<Duration> {
	cyborgtime::parse_duration(duration)
		.map_err(|error| err!("'{duration:?}' is not a valid duration string: {error:?}"))
}

/// Checks whether a system time is at or before the current wall-clock time.
///
/// Equality is considered passed. A time later than the sampled current time
/// returns `false`.
#[inline]
#[must_use]
pub fn timepoint_has_passed(timepoint: SystemTime) -> bool {
	SystemTime::now()
		.duration_since(timepoint)
		.is_ok()
}

/// Formats a signed Unix timestamp as RFC 2822 text in UTC.
///
/// Timestamps outside Chrono's supported range use its default UTC date and
/// time before formatting.
#[must_use]
pub fn rfc2822_from_seconds(epoch: i64) -> String {
	use chrono::{DateTime, Utc};

	DateTime::<Utc>::from_timestamp(epoch, 0)
		.unwrap_or_default()
		.to_rfc2822()
}

/// Formats a system time in UTC with a Chrono format string.
///
/// The pattern is passed to Chrono without modification. The rendered value is
/// returned as an owned string.
#[must_use]
pub fn format(ts: SystemTime, str: &str) -> String {
	use chrono::{DateTime, Utc};

	let dt: DateTime<Utc> = ts.into();
	dt.format(str).to_string()
}

/// Formats a duration with one plural human-readable unit.
///
/// The unit and scale component come from [`whole_and_frac`]. Output has the
/// form `{whole}.{scaled} {unit}`, where `scaled` is the component multiplied
/// by 100, truncated to an integer, and emitted without zero padding.
#[must_use]
#[expect(
	clippy::as_conversions,
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss
)]
pub fn pretty(d: Duration) -> String {
	use Unit::*;

	let fmt = |w, f, u| format!("{w}.{f} {u}");
	let gen64 = |w, f, u| fmt(w, (f * 100.0) as u32, u);
	let gen128 = |w, f, u| gen64(u64::try_from(w).expect("u128 to u64"), f, u);
	match whole_and_frac(d) {
		| (Days(whole), frac) => gen64(whole, frac, "days"),
		| (Hours(whole), frac) => gen64(whole, frac, "hours"),
		| (Mins(whole), frac) => gen64(whole, frac, "minutes"),
		| (Secs(whole), frac) => gen64(whole, frac, "seconds"),
		| (Millis(whole), frac) => gen128(whole, frac, "milliseconds"),
		| (Micros(whole), frac) => gen128(whole, frac, "microseconds"),
		| (Nanos(whole), frac) => gen128(whole, frac, "nanoseconds"),
	}
}

/// Pairs a duration's selected whole unit with a floating-point scale
/// component.
///
/// For days through minutes, the second value is a whole-second remainder
/// divided by the selected unit, so subsecond data is discarded. Seconds use
/// milliseconds within the current second, discarding submillisecond data;
/// milliseconds use microseconds, discarding nanoseconds; microseconds use
/// nanoseconds; nanoseconds return `0.0`.
#[must_use]
#[expect(clippy::as_conversions, clippy::cast_precision_loss)]
pub fn whole_and_frac(d: Duration) -> (Unit, f64) {
	use Unit::*;

	let whole = whole_unit(d);
	(whole, match whole {
		| Days(_) => (d.as_secs() % 86_400) as f64 / 86_400.0,
		| Hours(_) => (d.as_secs() % 3_600) as f64 / 3_600.0,
		| Mins(_) => (d.as_secs() % 60) as f64 / 60.0,
		| Secs(_) => f64::from(d.subsec_millis()) / 1000.0,
		| Millis(_) => f64::from(d.subsec_micros()) / 1000.0,
		| Micros(_) => f64::from(d.subsec_nanos()) / 1000.0,
		| Nanos(_) => 0.0,
	})
}

/// Selects the largest integral unit represented by a duration.
///
/// The stored value is rounded down to a whole unit. A zero duration is
/// represented as `Unit::Nanos(0)`.
#[must_use]
pub fn whole_unit(d: Duration) -> Unit {
	use Unit::*;

	match d.as_secs() {
		| 86_400.. => Days(d.as_secs() / 86_400),
		| 3_600..=86_399 => Hours(d.as_secs() / 3_600),
		| 60..=3_599 => Mins(d.as_secs() / 60),
		| _ => match d.as_micros() {
			| 1_000_000.. => Secs(d.as_secs()),
			| 1_000..=999_999 => Millis(d.subsec_millis().into()),
			| _ => match d.as_nanos() {
				| 1_000.. => Micros(d.subsec_micros().into()),
				| _ => Nanos(d.subsec_nanos().into()),
			},
		},
	}
}

/// Represents an integral duration in one selected unit.
///
/// Each variant stores the whole count for its named unit. [`whole_unit`]
/// selects the largest unit with a nonzero count, except that zero is
/// represented in nanoseconds.
#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum Unit {
	/// A duration measured in whole 86,400-second days.
	///
	/// [`whole_unit`] selects this variant for durations of at least one day.
	Days(u64),

	/// A duration measured in whole hours.
	///
	/// [`whole_unit`] selects this variant below one day and at or above one
	/// hour.
	Hours(u64),

	/// A duration measured in whole minutes.
	///
	/// [`whole_unit`] selects this variant below one hour and at or above one
	/// minute.
	Mins(u64),

	/// A duration measured in whole seconds.
	///
	/// [`whole_unit`] selects this variant below one minute and at or above one
	/// second.
	Secs(u64),

	/// A duration measured in whole milliseconds.
	///
	/// [`whole_unit`] selects this variant below one second and at or above one
	/// millisecond.
	Millis(u128),

	/// A duration measured in whole microseconds.
	///
	/// [`whole_unit`] selects this variant below one millisecond and at or
	/// above one microsecond.
	Micros(u128),

	/// A duration measured in whole nanoseconds.
	///
	/// [`whole_unit`] selects this variant below one microsecond, including for
	/// zero.
	Nanos(u128),
}
