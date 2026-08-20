use std::{
	fmt::{Display, Formatter, Result as FmtResult},
	time::Duration,
};

/// A compact, truncated display representation of an elapsed duration.
///
/// The value uses seconds, milliseconds, microseconds, or nanoseconds as
/// appropriate. Fractional values retain at most two decimal places.
#[derive(Clone, Copy, Debug)]
pub struct Elapsed(Duration);

impl Display for Elapsed {
	fn fmt(&self, formatter: &mut Formatter<'_>) -> FmtResult {
		let (hundredths, unit) = match self.0 {
			| duration if duration.as_secs() > 0 => (duration.as_millis() / 10, "s"),
			| duration if duration.as_millis() > 0 => (duration.as_micros() / 10, "ms"),
			| duration if duration.as_micros() > 0 => (duration.as_nanos() / 10, "µs"),
			| duration => return write!(formatter, "{}ns", duration.as_nanos()),
		};

		let whole = hundredths / 100;
		let fraction = hundredths % 100;

		match fraction {
			| 0 => write!(formatter, "{whole}{unit}"),
			| fraction if fraction % 10 == 0 =>
				write!(formatter, "{whole}.{}{unit}", fraction / 10),
			| _ => write!(formatter, "{whole}.{fraction:02}{unit}"),
		}
	}
}

impl From<Duration> for Elapsed {
	fn from(duration: Duration) -> Self { Self(duration) }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn formats_units_and_truncates_fractions() {
		let cases = [
			(Duration::ZERO, "0ns"),
			(Duration::from_nanos(999), "999ns"),
			(Duration::from_micros(1), "1µs"),
			(Duration::from_nanos(1_559), "1.55µs"),
			(Duration::from_nanos(999_999), "999.99µs"),
			(Duration::from_millis(1), "1ms"),
			(Duration::from_nanos(12_559_999), "12.55ms"),
			(Duration::from_nanos(999_999_999), "999.99ms"),
			(Duration::from_secs(1), "1s"),
			(Duration::from_millis(1_500), "1.5s"),
			(Duration::from_nanos(1_559_999_999), "1.55s"),
		];

		for (duration, expected) in cases {
			assert_eq!(Elapsed::from(duration).to_string(), expected);
		}
	}
}
