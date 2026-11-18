use ruma::{CanonicalJsonObject, CanonicalJsonValue};
use serde::{Deserialize, Deserializer, Serialize, de::IgnoredAny};

use super::Event;

/// The longest an event stays sticky, in milliseconds (MSC4354).
///
/// A longer duration is clamped to it, both where a client requests one and
/// where an event's expiry is computed.
pub const STICKY_DURATION_MAX: u64 = 3_600_000;

/// Unstable name of an event's top-level `sticky` object (MSC4354).
///
/// The object is part of the hashed and signed event, unlike the remaining
/// time a client copy carries beside it.
pub const STICKY_KEY: &str = "msc4354_sticky";

/// Unstable name of the remaining sticky time in a client copy's `unsigned`.
///
/// Each copy computes it when formatted, from the event's timestamp and
/// duration, so it never travels over federation.
pub const STICKY_TTL_KEY: &str = "msc4354_sticky_duration_ttl_ms";

/// How far past our clock an event may be dated and still be sticky.
///
/// MSC4354 starts stickiness at receipt so a future timestamp cannot extend
/// it; an event dated beyond this cutoff is never sticky instead, which bounds
/// what a forged timestamp gains to the cutoff itself.
const FUTURE_CUTOFF: u64 = 300_000;

/// An event's top-level MSC4354 `sticky` object.
///
/// The duration is kept as the event carries it. The one-hour bound applies
/// where expiry is computed, so every server orders a remote event's copies on
/// the same raw value.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Sticky {
	/// Milliseconds the sender asked the event to stay sticky.
	pub duration_ms: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Lenient {
	/// Serde reads a struct from a sequence as well; MSC4354 allows an object
	/// alone, so a sequence is caught here before it can read as valid.
	Sequence(#[serde(deserialize_with = "sequence")] ()),
	Valid(Sticky),
	Invalid(IgnoredAny),
}

/// Deserializes an optional `Sticky`, reading a malformed object as absent.
///
/// A remote's malformed sticky object makes its event non-sticky rather than
/// failing the whole PDU.
pub(crate) fn deserialize_lenient<'de, D>(deserializer: D) -> Result<Option<Sticky>, D::Error>
where
	D: Deserializer<'de>,
{
	Option::deserialize(deserializer).map(|lenient: Option<Lenient>| {
		lenient.and_then(|lenient| match lenient {
			| Lenient::Valid(sticky) => Some(sticky),
			| Lenient::Sequence(()) | Lenient::Invalid(_) => None,
		})
	})
}

fn sequence<'de, D>(deserializer: D) -> Result<(), D::Error>
where
	D: Deserializer<'de>,
{
	deserializer.deserialize_seq(IgnoredAny).map(drop)
}

pub(super) fn event_remaining<E: Event>(event: &E, now: u64) -> Option<u64> {
	event_until(event, now).map(|until| remaining_until(until, now))
}

pub(super) fn event_until<E: Event>(event: &E, now: u64) -> Option<u64> {
	let start = u64::from(event.origin_server_ts().get());

	event
		.sticky()
		.and_then(|sticky| expiry(start, sticky.duration_ms, now))
}

/// The remaining sticky time of an event copy nested in another event's
/// `unsigned`, read from the copy's own timestamp and sticky object.
///
/// A thread's latest event or a redaction's copy is sticky on its own terms,
/// so its remaining time is never its parent's.
pub(super) fn nested_remaining(event: &CanonicalJsonObject, now: u64) -> Option<u64> {
	let integer = |value: &CanonicalJsonValue| {
		value
			.as_integer()
			.and_then(|int| u64::try_from(i64::from(int)).ok())
	};

	let start = event.get("origin_server_ts").and_then(integer)?;
	let duration = event
		.get(STICKY_KEY)
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|sticky| sticky.get("duration_ms"))
		.and_then(integer)?;

	expiry(start, duration, now).map(|until| remaining_until(until, now))
}

fn expiry(start: u64, duration_ms: u64, now: u64) -> Option<u64> {
	let duration = duration_ms.min(STICKY_DURATION_MAX);
	let sticky = duration > 0 && start <= now.saturating_add(FUTURE_CUTOFF);

	sticky
		.then(|| start.saturating_add(duration))
		.filter(|&until| until > now)
}

fn remaining_until(until: u64, now: u64) -> u64 {
	until.saturating_sub(now).min(STICKY_DURATION_MAX)
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::{FUTURE_CUTOFF, STICKY_DURATION_MAX, expiry, nested_remaining, remaining_until};
	use crate::PduEvent;

	const NOW: u64 = 1_838_188_000;

	#[test]
	fn expiry_starts_at_the_timestamp() {
		assert_eq!(expiry(NOW, 60_000, NOW), Some(NOW + 60_000));
		assert_eq!(expiry(NOW - 30_000, 60_000, NOW), Some(NOW + 30_000));
	}

	#[test]
	fn expiry_clamps_a_duration_over_one_hour() {
		assert_eq!(expiry(NOW, 7_200_000, NOW), Some(NOW + STICKY_DURATION_MAX));
	}

	#[test]
	fn zero_duration_is_never_sticky() {
		assert_eq!(expiry(NOW, 0, NOW), None);
	}

	#[test]
	fn expired_event_is_not_sticky() {
		assert_eq!(expiry(NOW - 60_000, 60_000, NOW), None);
		assert_eq!(expiry(NOW - 60_001, 60_000, NOW), None);
	}

	#[test]
	fn future_timestamp_is_sticky_only_within_the_cutoff() {
		let edge = NOW + FUTURE_CUTOFF;

		assert_eq!(expiry(edge, 60_000, NOW), Some(edge + 60_000));
		assert_eq!(expiry(edge + 1, 60_000, NOW), None);
	}

	#[test]
	fn remaining_time_is_capped_at_one_hour() {
		let until = NOW + FUTURE_CUTOFF + STICKY_DURATION_MAX;

		assert_eq!(remaining_until(until, NOW), STICKY_DURATION_MAX);
		assert_eq!(remaining_until(NOW + 1, NOW), 1);
	}

	#[test]
	fn nested_copy_reads_its_own_timestamp() {
		let copy = json!({
			"event_id": "$latest:example.com",
			"origin_server_ts": NOW - 100_000,
			"msc4354_sticky": { "duration_ms": 600_000 },
		});

		let copy = serde_json::from_value(copy).expect("valid canonical object");

		assert_eq!(nested_remaining(&copy, NOW), Some(500_000));
		assert_eq!(nested_remaining(&copy, NOW + 500_000), None);
	}

	#[test]
	fn malformed_sticky_object_reads_as_absent() {
		let event = |sticky: serde_json::Value| {
			let event = json!({
				"type": "m.room.message",
				"content": {},
				"event_id": "$event:example.com",
				"room_id": "!room:example.com",
				"sender": "@sender:example.com",
				"prev_events": [],
				"auth_events": [],
				"origin_server_ts": NOW,
				"depth": 1,
				"hashes": { "sha256": "thishashcoversallfieldsincasethisisredacted" },
				"msc4354_sticky": sticky,
			});

			serde_json::from_value::<PduEvent>(event).expect("the event parses either way")
		};

		let valid = event(json!({ "duration_ms": 5_000 })).sticky;

		assert_eq!(event(json!({ "duration_ms": "soon" })).sticky, None);
		assert_eq!(event(json!(5)).sticky, None);
		assert_eq!(event(json!([5_000])).sticky, None);
		assert_eq!(valid.map(|sticky| sticky.duration_ms), Some(5_000));
	}
}
