use futures::{StreamExt, pin_mut};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId,
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{Result, err, implement, result::FlatOk};
use tuwunel_matrix::event::gen_event_id;

use super::room_version_of;

type Parsed = (OwnedRoomId, OwnedEventId, CanonicalJsonObject);

/// Parses an incoming event and derives its ID under its room's version.
///
/// An event for a room being joined waits on the room's federation lock until
/// the join applies, so a caller never holds or acquires that lock while
/// parsing.
#[implement(super::Service)]
#[tracing::instrument(
    name = "parse_incoming",
    level = "trace",
    skip_all,
    fields(
        len = pdu.get().len(),
    )
)]
pub async fn parse_incoming_pdu(&self, pdu: &RawJsonValue) -> Result<Parsed> {
	let value: CanonicalJsonObject = serde_json::from_str(pdu.get()).map_err(|e| {
		err!(BadServerResponse(debug_error!("Error parsing incoming event: {e} {pdu:#?}")))
	})?;

	let room_id: OwnedRoomId = value
		.get("room_id")
		.and_then(CanonicalJsonValue::as_str)
		.map(OwnedRoomId::parse)
		.flat_ok_or(err!(Request(InvalidParam("Invalid room_id in pdu"))))?;

	let room_version_id = self
		.incoming_room_version(&room_id)
		.await
		.ok_or_else(|| err!("Server is not in room {room_id}"))?;

	gen_event_id(&value, &room_version_id)
		.map(move |event_id| (room_id, event_id, value))
		.map_err(|e| {
			err!(Request(InvalidParam("Could not convert event to canonical json: {e}")))
		})
}

/// The version of the room an incoming event names, if this server can know it.
///
/// A room this server is joining for the first time has no version stored
/// until the join response is applied, and the remote join holds the room's
/// federation lock until then; an event pushed in that window, such as a
/// resident's sticky events (MSC4354), waits for the join instead of being
/// dropped. The version is read again after any wait, since a join that
/// released the lock before it was seen has installed it already. Failing
/// that, a locally-invited member's stripped state recovers the version of a
/// room we are not resident in.
#[implement(super::Service)]
async fn incoming_room_version(&self, room_id: &OwnedRoomId) -> Option<RoomVersionId> {
	let stored = async || {
		self.services
			.state
			.get_room_version(room_id)
			.await
			.ok()
	};

	if let Some(room_version) = stored().await {
		return Some(room_version);
	}

	if self.mutex_federation.contains(room_id) {
		drop(self.mutex_federation.lock(room_id).await);
	}

	if let Some(room_version) = stored().await {
		return Some(room_version);
	}

	// We may not be resident (e.g. a rescinded out-of-band invite).
	self.invited_room_version(room_id).await
}

/// Recover a room's version from a locally-invited member's stored stripped
/// state, for a room we are not resident in (e.g. a rescinded out-of-band
/// invite). The create event in the stripped state carries the version.
#[implement(super::Service)]
async fn invited_room_version(&self, room_id: &RoomId) -> Option<RoomVersionId> {
	let invited = self
		.services
		.state_cache
		.room_members_invited(room_id)
		.map(ToOwned::to_owned);

	pin_mut!(invited);
	while let Some(user_id) = invited.next().await {
		if self.services.globals.user_is_local(&user_id)
			&& let Ok(stripped) = self
				.services
				.state_cache
				.invite_state(&user_id, room_id)
				.await
			&& let Some(room_version) = room_version_of(&stripped)
		{
			return Some(room_version);
		}
	}

	None
}
