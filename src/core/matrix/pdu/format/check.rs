use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, ID_MAX_BYTES, RoomId, int,
	room_version_rules::EventFormatRules,
};
use serde_json::to_string as to_json_string;

use crate::{
	Err, Result, err,
	matrix::pdu::{MAX_AUTH_EVENTS, MAX_PDU_BYTES, MAX_PREV_EVENTS},
};

/// Check that the given canonicalized PDU respects the event format of the room
/// version and the [size limits] from the Matrix specification.
///
/// This is part of the [checks performed on receipt of a PDU].
///
/// This checks the following and enforces their size limits:
///
/// * Full PDU
/// * `sender`
/// * `room_id`
/// * `type`
/// * `event_id`
/// * `state_key`
/// * `prev_events`
/// * `auth_events`
/// * `depth`
///
/// Returns an `Err(_)` if the JSON is malformed or if the PDU doesn't pass the
/// checks.
///
/// [size limits]: https://spec.matrix.org/latest/client-server-api/#size-limits
/// [checks performed on receipt of a PDU]: https://spec.matrix.org/latest/server-server-api/#checks-performed-on-receipt-of-a-pdu
pub fn check_pdu_format(pdu: &CanonicalJsonObject, rules: &EventFormatRules) -> Result {
	// Check the PDU size, it must occur on the full PDU with signatures.
	let json = to_json_string(&pdu)
		.map_err(|e| err!(Request(BadJson("Failed to serialize canonical JSON: {e}"))))?;

	if json.len() > MAX_PDU_BYTES {
		return Err!(Request(TooLarge("PDU is larger than maximum of {MAX_PDU_BYTES} bytes")));
	}

	// Check the presence, type and length of the `type` field.
	let event_type = extract_required_string_field(pdu, "type")?;

	// Check the presence, type and length of the `sender` field.
	extract_required_string_field(pdu, "sender")?;

	// Check the presence, type and length of the `room_id` field.
	let room_id = (event_type != "m.room.create" || rules.require_room_create_room_id)
		.then(|| extract_required_string_field(pdu, "room_id"))
		.transpose()?;

	// Check the presence, type and length of the `event_id` field.
	if rules.require_event_id {
		extract_required_string_field(pdu, "event_id")?;
	}

	// Check the type and length of the `state_key` field.
	extract_optional_string_field(pdu, "state_key")?;

	// Check the presence, type and length of the `prev_events` field.
	extract_required_array_field(pdu, "prev_events", MAX_PREV_EVENTS)?;

	// Check the presence, type and length of the `auth_events` field.
	let auth_events = extract_required_array_field(pdu, "auth_events", MAX_AUTH_EVENTS)?;

	if !rules.allow_room_create_in_auth_events {
		// The only case where the room ID should be missing is for m.room.create which
		// shouldn't have any auth_events.
		if let Some(room_id) = room_id {
			let room_create_event_reference_hash = <&RoomId>::try_from(room_id.as_str())
				.map_err(|e| err!("invalid `room_id` field in PDU: {e}"))?
				.strip_sigil();

			for event_id in auth_events {
				let CanonicalJsonValue::String(event_id) = event_id else {
					return Err!(Request(InvalidParam(
						"unexpected format of array item in `auth_events` field in PDU: \
						 expected string, got {event_id:?}"
					)));
				};

				let reference_hash =
					event_id
						.strip_prefix('$')
						.ok_or(err!(Request(InvalidParam(
							"unexpected format of array item in `auth_events` field in PDU: \
							 string not beginning with the `$` sigil"
						))))?;

				if reference_hash == room_create_event_reference_hash {
					return Err!(Request(InvalidParam(
						"invalid `auth_events` field in PDU: cannot contain the `m.room.create` \
						 event ID"
					)));
				}
			}
		}
	}

	// Check the presence, type and value of the `depth` field.
	match pdu.get("depth") {
		| Some(CanonicalJsonValue::Integer(value)) =>
			if *value < int!(0) {
				return Err!(Request(InvalidParam(
					"invalid `depth` field in PDU: cannot be a negative integer"
				)));
			},
		| Some(value) => {
			return Err!(Request(InvalidParam(
				"unexpected format of `depth` field in PDU: expected integer, got {value:?}"
			)));
		},
		| None => return Err!(Request(InvalidParam("missing `depth` field in PDU"))),
	}

	Ok(())
}

/// Extract the optional string field with the given name from the given
/// canonical JSON object.
///
/// Returns `Ok(Some(value))` if the field is present and a valid string,
/// `Ok(None)` if the field is missing and `Err(_)` if the field is not a string
/// or its length is bigger than [`ID_MAX_BYTES`].
fn extract_optional_string_field<'a>(
	object: &'a CanonicalJsonObject,
	field: &'a str,
) -> Result<Option<&'a String>> {
	match object.get(field) {
		| Some(CanonicalJsonValue::String(value)) =>
			if value.len() > ID_MAX_BYTES {
				Err!(Request(TooLarge(
					"invalid `{field}` field in PDU: string length is larger than maximum of \
					 {ID_MAX_BYTES} bytes"
				)))
			} else {
				Ok(Some(value))
			},

		| Some(value) => Err!(Request(InvalidParam(
			"unexpected format of `{field}` field in PDU: expected string, got {value:?}"
		))),

		| None => Ok(None),
	}
}

/// Extract the required string field with the given name from the given
/// canonical JSON object.
///
/// Returns `Ok(value)` if the field is present and a valid string and `Err(_)`
/// if the field is missing, not a string or its length is bigger than
/// [`ID_MAX_BYTES`].
fn extract_required_string_field<'a>(
	object: &'a CanonicalJsonObject,
	field: &'a str,
) -> Result<&'a String> {
	extract_optional_string_field(object, field)?
		.ok_or_else(|| err!(Request(InvalidParam("missing `{field}` field in PDU"))))
}

/// Extract the required array field with the given name from the given
/// canonical JSON object.
///
/// Returns `Ok(value)` if the field is present and a valid array or `Err(_)` if
/// the field is missing, not an array or its length is bigger than the given
/// value.
fn extract_required_array_field<'a>(
	object: &'a CanonicalJsonObject,
	field: &'a str,
	max_len: usize,
) -> Result<&'a [CanonicalJsonValue]> {
	match object.get(field) {
		| Some(CanonicalJsonValue::Array(value)) =>
			if value.len() > max_len {
				Err!(Request(TooLarge(
					"invalid `{field}` field in PDU: array length is larger than maximum of \
					 {max_len}"
				)))
			} else {
				Ok(value)
			},

		| Some(value) => Err!(Request(InvalidParam(
			"unexpected format of `{field}` field in PDU: expected array, got {value:?}"
		))),

		| None => Err!(Request(InvalidParam("missing `{field}` field in PDU"))),
	}
}

#[cfg(test)]
mod tests {
	use std::iter::repeat_n;

	use ruma::{
		CanonicalJsonObject, CanonicalJsonValue, int, room_version_rules::EventFormatRules,
	};
	use serde_json::{from_value as from_json_value, json};

	use super::check_pdu_format;

	/// Construct a PDU valid for the event format of room v1.
	fn pdu_v1() -> CanonicalJsonObject {
		let pdu = json!({
			"auth_events": [
				[
					"$af232176:example.org",
					{ "sha256": "abase64encodedsha256hashshouldbe43byteslong" },
				],
			],
			"content": {
				"key": "value",
			},
			"depth": 12,
			"event_id": "$a4ecee13e2accdadf56c1025:example.com",
			"hashes": {
				"sha256": "thishashcoversallfieldsincasethisisredacted"
			},
			"origin_server_ts": 1_838_188_000,
			"prev_events": [
				[
					"$af232176:example.org",
					{ "sha256": "abase64encodedsha256hashshouldbe43byteslong" }
				],
			],
			"room_id": "!UcYsUzyxTGDxLBEvLy:example.org",
			"sender": "@alice:example.com",
			"signatures": {
				"example.com": {
					"ed25519:key_version": "these86bytesofbase64signaturecoveressentialfieldsincludinghashessocancheckredactedpdus",
				},
			},
			"type": "m.room.message",
			"unsigned": {
				"age": 4612,
			},
		});

		from_json_value(pdu).unwrap()
	}

	/// Construct a PDU valid for the event format of room v3.
	fn pdu_v3() -> CanonicalJsonObject {
		let pdu = json!({
			"auth_events": [
				"$base64encodedeventid",
				"$adifferenteventid",
			],
			"content": {
				"key": "value",
			},
			"depth": 12,
			"hashes": {
				"sha256": "thishashcoversallfieldsincasethisisredacted",
			},
			"origin_server_ts": 1_838_188_000,
			"prev_events": [
				"$base64encodedeventid",
				"$adifferenteventid",
			],
			"redacts": "$some/old+event",
			"room_id": "!UcYsUzyxTGDxLBEvLy:example.org",
			"sender": "@alice:example.com",
			"signatures": {
				"example.com": {
					"ed25519:key_version": "these86bytesofbase64signaturecoveressentialfieldsincludinghashessocancheckredactedpdus",
				},
			},
			"type": "m.room.message",
			"unsigned": {
				"age": 4612,
			}
		});

		from_json_value(pdu).unwrap()
	}

	/// Construct an `m.room.create` PDU valid for the event format of
	/// `org.matrix.hydra.11`.
	fn room_create_hydra() -> CanonicalJsonObject {
		let pdu = json!({
			"auth_events": [],
			"content": {
				"room_version": "org.matrix.hydra.11",
			},
			"depth": 1,
			"hashes": {
				"sha256": "thishashcoversallfieldsincasethisisredacted",
			},
			"origin_server_ts": 1_838_188_000,
			"prev_events": [],
			"sender": "@alice:example.com",
			"signatures": {
				"example.com": {
					"ed25519:key_version": "these86bytesofbase64signaturecoveressentialfieldsincludinghashessocancheckredactedpdus",
				},
			},
			"type": "m.room.create",
			"unsigned": {
				"age": 4612,
			}
		});

		from_json_value(pdu).unwrap()
	}

	/// Construct a PDU valid for the event format of `org.matrix.hydra.11`.
	fn pdu_hydra() -> CanonicalJsonObject {
		let pdu = json!({
			"auth_events": [
				"$base64encodedeventid",
				"$adifferenteventid",
			],
			"content": {
				"key": "value",
			},
			"depth": 12,
			"hashes": {
				"sha256": "thishashcoversallfieldsincasethisisredacted",
			},
			"origin_server_ts": 1_838_188_000,
			"prev_events": [
				"$base64encodedeventid",
			],
			"room_id": "!roomcreatereferencehash",
			"sender": "@alice:example.com",
			"signatures": {
				"example.com": {
					"ed25519:key_version": "these86bytesofbase64signaturecoveressentialfieldsincludinghashessocancheckredactedpdus",
				},
			},
			"type": "m.room.message",
			"unsigned": {
				"age": 4612,
			}
		});

		from_json_value(pdu).unwrap()
	}

	#[test]
	fn check_pdu_format_valid_v1() {
		check_pdu_format(&pdu_v1(), &EventFormatRules::V1).unwrap();
	}

	#[test]
	fn check_pdu_format_valid_v3() {
		check_pdu_format(&pdu_v3(), &EventFormatRules::V3).unwrap();
	}

	#[test]
	fn check_pdu_format_pdu_too_big() {
		// Add a lot of data in the content to reach MAX_PDU_SIZE.
		let mut pdu = pdu_v3();
		let content = pdu
			.get_mut("content")
			.unwrap()
			.as_object_mut()
			.unwrap();

		let long_string = repeat_n('a', 66_000).collect::<String>();
		content.insert("big_data".into(), long_string.into());
		check_pdu_format(&pdu, &EventFormatRules::V3).unwrap_err();
	}

	#[test]
	fn check_pdu_format_fields_missing() {
		for field in
			&["event_id", "sender", "room_id", "type", "prev_events", "auth_events", "depth"]
		{
			let mut pdu = pdu_v1();
			pdu.remove(*field).unwrap();
			check_pdu_format(&pdu, &EventFormatRules::V1).unwrap_err();
		}
	}

	#[test]
	fn check_pdu_format_strings_too_big() {
		for field in &["event_id", "sender", "room_id", "type", "state_key"] {
			let mut pdu = pdu_v1();
			let value = repeat_n('a', 300).collect::<String>();
			pdu.insert((*field).into(), value.into());
			check_pdu_format(&pdu, &EventFormatRules::V1).unwrap_err();
		}
	}

	#[test]
	fn check_pdu_format_strings_wrong_format() {
		for field in &["event_id", "sender", "room_id", "type", "state_key"] {
			let mut pdu = pdu_v1();
			pdu.insert((*field).into(), true.into());
			check_pdu_format(&pdu, &EventFormatRules::V1).unwrap_err();
		}
	}

	#[test]
	fn check_pdu_format_arrays_too_big() {
		for field in &["prev_events", "auth_events"] {
			let mut pdu = pdu_v3();
			let value: Vec<_> =
				repeat_n(CanonicalJsonValue::from("$eventid".to_owned()), 30).collect();

			pdu.insert((*field).into(), value.into());
			check_pdu_format(&pdu, &EventFormatRules::V3).unwrap_err();
		}
	}

	#[test]
	fn check_pdu_format_arrays_wrong_format() {
		for field in &["prev_events", "auth_events"] {
			let mut pdu = pdu_v3();
			pdu.insert((*field).into(), true.into());
			check_pdu_format(&pdu, &EventFormatRules::V3).unwrap_err();
		}
	}

	#[test]
	fn check_pdu_format_negative_depth() {
		let mut pdu = pdu_v3();
		pdu.insert("depth".into(), int!(-1).into())
			.unwrap();

		check_pdu_format(&pdu, &EventFormatRules::V3).unwrap_err();
	}

	#[test]
	fn check_pdu_format_depth_wrong_format() {
		let mut pdu = pdu_v3();
		pdu.insert("depth".into(), true.into());
		check_pdu_format(&pdu, &EventFormatRules::V3).unwrap_err();
	}

	#[test]
	fn check_pdu_format_valid_room_create_hydra() {
		let pdu = room_create_hydra();
		check_pdu_format(&pdu, &EventFormatRules::V12).unwrap();
	}

	#[test]
	fn check_pdu_format_valid_hydra() {
		let pdu = pdu_hydra();
		check_pdu_format(&pdu, &EventFormatRules::V12).unwrap();
	}

	#[test]
	fn check_pdu_format_hydra_with_room_create() {
		let mut pdu = pdu_hydra();
		pdu.get_mut("auth_events")
			.unwrap()
			.as_array_mut()
			.unwrap()
			.push("$roomcreatereferencehash".to_owned().into());

		check_pdu_format(&pdu, &EventFormatRules::V12).unwrap_err();
	}
}
