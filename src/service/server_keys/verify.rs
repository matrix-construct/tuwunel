//! Event-ID derivation and federation signature verification.
//!
//! Verification resolves required signing keys through the server-key service
//! and applies room-version signature rules. The local-keys validator returns
//! an event in the form this server stores it, redacted when its content hash
//! does not match.

use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomVersionId,
	canonical_json::redact,
	room_version_rules::RoomVersionRules,
	signatures::{Verified, verify_event},
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{Err, Result, err, implement};
use tuwunel_matrix::{event::gen_event_id_canonical_json, room_version};

/// An event's derived ID, its stored form, and how it verified.
///
/// The stored form of a [`Verified::Signatures`] event is its redaction.
type Validated = (OwnedEventId, CanonicalJsonObject, Verified);

/// Derives and checks an event using only keys already in local storage.
///
/// The method rejects the event before verification when any required key is
/// absent. The stored form drops the sender's `unsigned` data and is redacted
/// when the content hash does not match, as for any other received event; newer
/// room versions then receive the derived `event_id`.
#[implement(super::Service)]
pub async fn validate_and_add_event_id_no_fetch(
	&self,
	pdu: &RawJsonValue,
	room_version_id: &RoomVersionId,
) -> Result<Validated> {
	let (event_id, value) = gen_event_id_canonical_json(pdu, room_version_id)?;
	let room_version_rules = room_version::rules(room_version_id)?;

	if !self
		.required_keys_exist(&value, &room_version_rules)
		.await
	{
		return Err!(BadServerResponse(debug_warn!(
			"Event {event_id} cannot be verified: missing keys."
		)));
	}

	let verified = self
		.verify_event(&value, Some(room_version_id))
		.await
		.map_err(|error| {
			err!(BadServerResponse(debug_error!(%event_id, ?error, "Event failed verification")))
		})?;

	stored_form(value, &event_id, &verified, &room_version_rules)
		.map(|value| (event_id, value, verified))
}

/// Verifies an event and returns ruma's verification classification.
///
/// Required keys are loaded or fetched through [`Self::get_event_keys`]. When
/// no room version is supplied, version 11 rules are used. Callers must inspect
/// [`Verified`] to distinguish complete verification from signatures only.
#[implement(super::Service)]
pub async fn verify_event(
	&self,
	event: &CanonicalJsonObject,
	room_version_id: Option<&RoomVersionId>,
) -> Result<Verified> {
	let room_version_id = room_version_id.unwrap_or(&RoomVersionId::V11);
	let room_version_rules = room_version::rules(room_version_id)?;

	let event_keys = self
		.get_event_keys(event, &room_version_rules)
		.await?;

	verify_event(&event_keys, event, &room_version_rules).map_err(Into::into)
}

/// Verifies signatures on a canonical JSON object.
///
/// Unlike [`Self::verify_event`], this does not verify an event content hash.
/// Version 11 signature rules are used when no room version is supplied.
#[implement(super::Service)]
pub async fn verify_json(
	&self,
	event: &CanonicalJsonObject,
	room_version_id: Option<&RoomVersionId>,
) -> Result {
	let room_version_id = room_version_id.unwrap_or(&RoomVersionId::V11);
	let room_version_rules = room_version::rules(room_version_id)?;

	let event_keys = self
		.get_event_keys(event, &room_version_rules)
		.await?;

	ruma::signatures::verify_json(&event_keys, event).map_err(Into::into)
}

/// Builds the stored form of a verified event from the received one.
///
/// The event ID was derived from the received form, so nothing done here can
/// change it.
fn stored_form(
	mut value: CanonicalJsonObject,
	event_id: &EventId,
	verified: &Verified,
	room_version_rules: &RoomVersionRules,
) -> Result<CanonicalJsonObject> {
	value.remove("unsigned");

	if *verified == Verified::Signatures {
		value = redact(value, &room_version_rules.redaction, None).map_err(|error| {
			err!(BadServerResponse(
				debug_error!(%event_id, %error, "Event could not be redacted")
			))
		})?;
	}

	// For v3+ rooms we add the event_id, but for v1/v2 rooms it's already present.
	if !room_version_rules.event_format.require_event_id {
		value.insert("event_id".into(), CanonicalJsonValue::String(event_id.as_str().into()));
	}

	Ok(value)
}
