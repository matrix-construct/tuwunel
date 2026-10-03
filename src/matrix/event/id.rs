use ruma::{CanonicalJsonObject, CanonicalJsonValue, OwnedEventId, RoomVersionId};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{Result, debug_error, err};

use crate::room_version;

/// Generates a correct eventId for the incoming pdu.
///
/// Returns a tuple of the new `EventId` and the PDU as a `BTreeMap<String,
/// CanonicalJsonValue>`.
#[tracing::instrument(
	name = "gen_event_id",
	level = "debug",
	skip_all,
	fields(
		len = pdu.get().len(),
	)
)]
pub fn gen_event_id_canonical_json(
	pdu: &RawJsonValue,
	room_version_id: &RoomVersionId,
) -> Result<(OwnedEventId, CanonicalJsonObject)> {
	let value: CanonicalJsonObject = serde_json::from_str(pdu.get())
		.map_err(|e| err!(BadServerResponse(warn!("Error parsing canonical event: {e}"))))
		.inspect_err(|e| debug_error!("{pdu:#?} {e:?}"))?;

	let event_id = gen_event_id(&value, room_version_id)?;

	Ok((event_id, value))
}

/// Generates a correct eventId for the PDU. For v1/v2 incoming PDU's the
/// value's event_id is passed through. For all outgoing PDU's and for v3+
/// incoming PDU's it is generated.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		members = value.len(),
		room_version = ?room_version_id,
	)
)]
pub fn gen_event_id(
	value: &CanonicalJsonObject,
	room_version_id: &RoomVersionId,
) -> Result<OwnedEventId> {
	let room_version_rules = room_version::rules(room_version_id)?;
	let require_event_id = room_version_rules.event_format.require_event_id;

	// We don't actually generate any event_id for incoming events in v1/v2 rooms,
	// just pass them through.
	if let Some(event_id) = require_event_id
		.then(|| value.get("event_id"))
		.flatten()
		.and_then(CanonicalJsonValue::as_str)
		.map(OwnedEventId::try_from)
		.transpose()?
	{
		return Ok(event_id);
	}

	// For outgoing v1/v2 add the server part. This has to be our origin but we
	// can't assert that here.
	let server_name = require_event_id
		.then(|| value.get("origin"))
		.flatten()
		.and_then(CanonicalJsonValue::as_str);

	let reference_hash = ruma::signatures::reference_hash(value, &room_version_rules)?;

	OwnedEventId::from_parts('$', &reference_hash, server_name).map_err(Into::into)
}
