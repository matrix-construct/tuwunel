use futures::{StreamExt, pin_mut};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId,
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{Result, err, implement, matrix::event::gen_event_id, result::FlatOk};

type Parsed = (OwnedRoomId, OwnedEventId, CanonicalJsonObject);

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

	let room_version_id = match self
		.services
		.state
		.get_room_version(&room_id)
		.await
	{
		| Ok(room_version_id) => room_version_id,
		// We may not be resident (e.g. a rescinded out-of-band invite); recover the
		// version from a locally-invited member's stored stripped state.
		| Err(_) => self
			.invited_room_version(&room_id)
			.await
			.ok_or_else(|| err!("Server is not in room {room_id}"))?,
	};

	gen_event_id(&value, &room_version_id)
		.map(move |event_id| (room_id, event_id, value))
		.map_err(|e| {
			err!(Request(InvalidParam("Could not convert event to canonical json: {e}")))
		})
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
			&& let Some(room_version) = super::room_version_of(&stripped)
		{
			return Some(room_version);
		}
	}

	None
}
