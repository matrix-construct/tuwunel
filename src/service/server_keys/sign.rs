//! Local signing and event-ID generation helpers.
//!
//! Events are content-hashed and signed with the server's active Ed25519 key.
//! Event-ID placement follows the selected room version's event format.

use ruma::{CanonicalJsonObject, CanonicalJsonValue, OwnedEventId, RoomVersionId};
use tuwunel_core::{
	Result, err, implement,
	matrix::{event::gen_event_id, room_version},
};

/// Generates an event ID, content hash, and local signature in place.
///
/// Any existing `event_id` is removed first. Legacy room versions generate and
/// insert the ID before signing; newer versions derive it after signing and then
/// insert it into the returned object.
#[implement(super::Service)]
pub fn gen_id_hash_and_sign_event(
	&self,
	object: &mut CanonicalJsonObject,
	room_version_id: &RoomVersionId,
) -> Result<OwnedEventId> {
	object.remove("event_id");

	if room_version::rules(room_version_id)?
		.event_format
		.require_event_id
	{
		self.gen_id_hash_and_sign_event_v1(object, room_version_id)
	} else {
		self.gen_id_hash_and_sign_event_v3(object, room_version_id)
	}
}

/// Generates and inserts an event ID before signing a legacy-format event.
///
/// The explicit ID participates in the content hash and signature for room
/// versions whose event format requires it.
#[implement(super::Service)]
fn gen_id_hash_and_sign_event_v1(
	&self,
	object: &mut CanonicalJsonObject,
	room_version_id: &RoomVersionId,
) -> Result<OwnedEventId> {
	let event_id = gen_event_id(object, room_version_id)?;

	object.insert("event_id".into(), CanonicalJsonValue::String(event_id.clone().into()));

	self.services
		.server_keys
		.hash_and_sign_event(object, room_version_id)?;

	Ok(event_id)
}

/// Signs a modern-format event before deriving and inserting its event ID.
///
/// The derived ID therefore reflects the signed event representation used by
/// room versions that omit an explicit ID during signing.
#[implement(super::Service)]
fn gen_id_hash_and_sign_event_v3(
	&self,
	object: &mut CanonicalJsonObject,
	room_version_id: &RoomVersionId,
) -> Result<OwnedEventId> {
	self.services
		.server_keys
		.hash_and_sign_event(object, room_version_id)?;

	let event_id = gen_event_id(object, room_version_id)?;

	object.insert("event_id".into(), CanonicalJsonValue::String(event_id.clone().into()));

	Ok(event_id)
}

/// Adds a content hash and local server signature to an event object.
///
/// Signing uses the room version's redaction rules. Oversized PDUs are mapped to
/// a request-too-large error and other signing failures to an unknown request
/// error.
#[implement(super::Service)]
pub fn hash_and_sign_event(
	&self,
	object: &mut CanonicalJsonObject,
	room_version_id: &RoomVersionId,
) -> Result {
	use ruma::signatures::{add_content_hash_to_event, sign_event};

	let server_name = &self.services.server.name;
	let room_version_rules = room_version::rules(room_version_id)?;

	let map_err = |e: ruma::signatures::JsonError| {
		use ruma::signatures::JsonError::PduTooLarge;
		match e {
			| PduTooLarge => {
				err!(Request(TooLarge("PDU exceeds 65535 bytes")))
			},
			| _ => err!(Request(Unknown(warn!("Signing event failed: {e}")))),
		}
	};

	add_content_hash_to_event(object).map_err(map_err)?;
	sign_event(server_name.as_str(), self.keypair(), object, &room_version_rules.redaction)
		.map_err(map_err)
}

/// Signs an arbitrary canonical JSON object with the local server key.
///
/// The signature is inserted under the configured local server name without
/// adding an event content hash.
#[implement(super::Service)]
pub fn sign_json(&self, object: &mut CanonicalJsonObject) -> Result {
	use ruma::signatures::sign_json;

	let server_name = self.services.globals.server_name().as_str();

	sign_json(server_name, self.keypair(), object).map_err(Into::into)
}
