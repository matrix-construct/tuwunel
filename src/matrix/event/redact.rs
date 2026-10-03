use ruma::{
	OwnedEventId,
	events::{TimelineEventType, room::redaction::RoomRedactionEventContent},
	room_version_rules::RoomVersionRules,
};
use serde::Deserialize;
use serde_json::value::to_raw_value;

use super::Event;
use crate::pdu::Content;

/// Copies the `redacts` property of the event to the `content` dict and
/// vice-versa.
///
/// This follows the specification's
/// [recommendation](https://spec.matrix.org/v1.10/rooms/v11/#moving-the-redacts-property-of-mroomredaction-events-to-a-content-property):
///
/// > For backwards-compatibility with older clients, servers should add a
/// > redacts property to the top level of m.room.redaction events in when
/// > serving such events over the Client-Server API.
///
/// > For improved compatibility with newer clients, servers should add a
/// > redacts property to the content of m.room.redaction events in older
/// > room versions when serving such events over the Client-Server API.
#[must_use]
pub(super) fn copy<E: Event>(event: &E) -> (Option<OwnedEventId>, Content) {
	if *event.event_type() != TimelineEventType::RoomRedaction {
		return (
			event.redacts().map(ToOwned::to_owned),
			Content::from_raw_value(event.content()),
		);
	}

	let Ok(mut content) = event.get_content::<RoomRedactionEventContent>() else {
		return (
			event.redacts().map(ToOwned::to_owned),
			Content::from_raw_value(event.content()),
		);
	};

	if let Some(redacts) = content.redacts {
		return (Some(redacts), Content::from_raw_value(event.content()));
	}

	if let Some(redacts) = event.redacts().map(ToOwned::to_owned) {
		content.redacts = Some(redacts);
		return (
			event.redacts().map(ToOwned::to_owned),
			Content::from_json(
				to_raw_value(&content).expect("Must be valid, we only added redacts field"),
			),
		);
	}

	(event.redacts().map(ToOwned::to_owned), Content::from_raw_value(event.content()))
}

#[must_use]
pub(super) fn is_redacted<E: Event>(event: &E) -> bool {
	let Some(unsigned) = event.unsigned() else {
		return false;
	};

	let Ok(unsigned) = ExtractRedactedBecause::deserialize(unsigned) else {
		return false;
	};

	unsigned.redacted_because.is_some()
}

#[must_use]
pub(super) fn redacts_id<E: Event>(
	event: &E,
	room_rules: &RoomVersionRules,
) -> Option<OwnedEventId> {
	if *event.kind() != TimelineEventType::RoomRedaction {
		return None;
	}

	if room_rules.redaction.content_field_redacts {
		event
			.get_content::<RoomRedactionEventContent>()
			.ok()?
			.redacts
	} else {
		event.redacts().map(ToOwned::to_owned)
	}
}

#[derive(Deserialize)]
struct ExtractRedactedBecause {
	redacted_because: Option<serde::de::IgnoredAny>,
}
