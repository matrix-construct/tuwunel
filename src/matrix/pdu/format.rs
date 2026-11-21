pub(super) mod check;
#[cfg(test)]
mod tests;

use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, RoomId, RoomVersionId, ServerName,
	room_version_rules::{EventsReferenceFormatVersion, RoomVersionRules},
};
use tuwunel_core::{extract_variant, is_equal_to};

use crate::room_version;

/// Converts a stored PDU object to its federation wire representation.
///
/// Unsigned data is never sent: the signatures do not cover it, no peer reads
/// the fields stored there, and they hold server aggregates such as a thread's
/// newest reply. Room-version rules select the fields and event-reference shape
/// sent to peers. When rules are unavailable, the event ID is removed without
/// rewriting event references.
#[must_use]
pub fn into_outgoing_federation(
	mut pdu_json: CanonicalJsonObject,
	room_version: &RoomVersionId,
) -> CanonicalJsonObject {
	pdu_json.remove("unsigned");

	let Ok(room_rules) = room_version::rules(room_version) else {
		pdu_json.remove("event_id");
		return pdu_json;
	};

	if !room_rules.event_format.require_event_id {
		pdu_json.remove("event_id");
	}

	if !room_rules
		.event_format
		.require_room_create_room_id
		&& pdu_json
			.get("type")
			.and_then(CanonicalJsonValue::as_str)
			.is_some_and(is_equal_to!("m.room.create"))
	{
		pdu_json.remove("room_id");
	}

	if matches!(room_rules.events_reference_format, EventsReferenceFormatVersion::V1) {
		if let Some(value) = pdu_json.get_mut("auth_events") {
			mutate_outgoing_reference_format(value);
		}
		if let Some(value) = pdu_json.get_mut("prev_events") {
			mutate_outgoing_reference_format(value);
		}
	}

	pdu_json
}

fn mutate_outgoing_reference_format(value: &mut CanonicalJsonValue) {
	value
		.as_array_mut()
		.into_iter()
		.flatten()
		.for_each(|value| {
			if let Some(event_id) = value.as_str().map(ToOwned::to_owned) {
				*value = CanonicalJsonValue::Array(vec![
					CanonicalJsonValue::String(event_id),
					CanonicalJsonValue::Object([(Default::default(), "".into())].into()),
				]);
			}
		});
}

/// Normalizes a federation PDU object into the stored representation.
///
/// Legacy event-reference tuples are collapsed and omitted room or event IDs
/// are restored from trusted request context. The supplied rules must match the
/// event's room version.
#[must_use]
pub fn from_incoming_federation(
	room_id: &RoomId,
	event_id: &EventId,
	mut pdu_json: CanonicalJsonObject,
	room_rules: &RoomVersionRules,
) -> CanonicalJsonObject {
	if matches!(room_rules.events_reference_format, EventsReferenceFormatVersion::V1) {
		if let Some(value) = pdu_json.get_mut("auth_events") {
			mutate_incoming_reference_format(value);
		}
		if let Some(value) = pdu_json.get_mut("prev_events") {
			mutate_incoming_reference_format(value);
		}
	}

	if !room_rules
		.event_format
		.require_room_create_room_id
		&& pdu_json["type"] == "m.room.create"
	{
		pdu_json
			.entry("room_id".into())
			.or_insert_with(|| CanonicalJsonValue::String(room_id.as_str().into()));
	}

	if !room_rules.event_format.require_event_id {
		pdu_json.insert("event_id".into(), CanonicalJsonValue::String(event_id.into()));
	}

	pdu_json
}

/// Adds one server's signatures to a PDU object.
///
/// Signatures the object already holds are kept, those of this server
/// included, and a held key wins over the same key added. An object without a
/// `signatures` map is returned unchanged.
#[must_use]
pub fn with_server_signatures(
	mut pdu_json: CanonicalJsonObject,
	server: &ServerName,
	signatures: CanonicalJsonObject,
) -> CanonicalJsonObject {
	let Some(CanonicalJsonValue::Object(all)) = pdu_json.get_mut("signatures") else {
		return pdu_json;
	};

	match all.get_mut(server.as_str()) {
		| Some(CanonicalJsonValue::Object(held)) =>
			for (key_id, signature) in signatures {
				held.entry(key_id).or_insert(signature);
			},
		| _ => {
			all.insert(server.as_str().into(), CanonicalJsonValue::Object(signatures));
		},
	}

	pdu_json
}

/// Replaces a PDU object's signatures with one server's.
///
/// Signatures are excluded from the signed bytes, so the result verifies against
/// that server's keys alone, where a verifier would otherwise demand keys for
/// every signer present.
#[must_use]
pub fn with_only_server_signatures(
	mut pdu_json: CanonicalJsonObject,
	server: &ServerName,
	signatures: CanonicalJsonObject,
) -> CanonicalJsonObject {
	let all = [(server.as_str().into(), CanonicalJsonValue::Object(signatures))];

	pdu_json.insert("signatures".into(), CanonicalJsonValue::Object(all.into()));
	pdu_json
}

/// Adds entries to a PDU object's unsigned data.
///
/// The entries extend an existing `unsigned` object, replacing keys it already
/// holds; a missing or malformed `unsigned` is set to them.
#[must_use]
pub fn with_unsigned(
	mut pdu_json: CanonicalJsonObject,
	unsigned: CanonicalJsonObject,
) -> CanonicalJsonObject {
	match pdu_json.get_mut("unsigned") {
		| Some(CanonicalJsonValue::Object(stored)) => stored.extend(unsigned),
		| _ => {
			pdu_json.insert("unsigned".into(), CanonicalJsonValue::Object(unsigned));
		},
	}

	pdu_json
}

fn mutate_incoming_reference_format(value: &mut CanonicalJsonValue) {
	value
		.as_array_mut()
		.into_iter()
		.flat_map(|vec| vec.iter_mut())
		.for_each(|value| {
			let event_id = value
				.as_array()
				.into_iter()
				.find_map(|vec| vec.first())
				.and_then(|val| extract_variant!(val, CanonicalJsonValue::String))
				.cloned()
				.unwrap_or_default();

			*value = CanonicalJsonValue::String(event_id);
		});
}
