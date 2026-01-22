pub mod exponential_backoff;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{Result, err};

#[inline]
#[must_use]
#[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
pub fn now_millis() -> u64 { now().as_millis() as u64 }

#[inline]
#[must_use]
pub fn now_secs() -> u64 { now().as_secs() }

#[inline]
#[must_use]
pub fn now() -> Duration {
	UNIX_EPOCH
		.elapsed()
		.expect("positive duration after epoch")
}

#[inline]
#[must_use]
pub fn duration_since_epoch(timepoint: SystemTime) -> Duration {
	timepoint
		.duration_since(UNIX_EPOCH)
		.unwrap_or(Duration::ZERO)
}

#[inline]
pub fn timepoint_from_epoch(duration: Duration) -> Result<SystemTime> {
	UNIX_EPOCH
		.checked_add(duration)
		.ok_or_else(|| err!(Arithmetic("Duration {duration:?} from epoch is too large")))
}

#[inline]
pub fn timepoint_from_now(duration: Duration) -> Result<SystemTime> {
	SystemTime::now()
		.checked_add(duration)
		.ok_or_else(|| err!(Arithmetic("Duration {duration:?} from now is too large")))
}

#[inline]
pub fn timepoint_ago(duration: Duration) -> Result<SystemTime> {
	SystemTime::now()
		.checked_sub(duration)
		.ok_or_else(|| err!(Arithmetic("Duration {duration:?} ago is too large")))
}

#[inline]
pub fn parse_timepoint_ago(ago: &str) -> Result<SystemTime> {
	timepoint_ago(parse_duration(ago)?)
}

#[inline]
pub fn parse_duration(duration: &str) -> Result<Duration> {
	cyborgtime::parse_duration(duration)
		.map_err(|error| err!("'{duration:?}' is not a valid duration string: {error:?}"))
}

#[inline]
#[must_use]
pub fn timepoint_has_passed(timepoint: SystemTime) -> bool {
	SystemTime::now()
		.duration_since(timepoint)
		.is_ok()
}

#[must_use]
pub fn rfc2822_from_seconds(epoch: i64) -> String {
	use chrono::{DateTime, Utc};

	DateTime::<Utc>::from_timestamp(epoch, 0)
		.unwrap_or_default()
		.to_rfc2822()
}

#[must_use]
pub fn format(ts: SystemTime, str: &str) -> String {
	use chrono::{DateTime, Utc};

	let dt: DateTime<Utc> = ts.into();
	dt.format(str).to_string()
}

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

/// Return a pair of (whole part, frac part) from a duration where. The whole
/// part is the largest Unit containing a non-zero value, the frac part is a
/// rational remainder left over.
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

/// Return the largest Unit which represents the duration. The value is
/// rounded-down, but never zero.
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

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum Unit {
	Days(u64),
	Hours(u64),
	Mins(u64),
	Secs(u64),
	Millis(u128),
	Micros(u128),
	Nanos(u128),
}
