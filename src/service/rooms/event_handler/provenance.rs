use ruma::{EventId, OwnedEventId, RoomId, events::{StateEventType, TimelineEventType}, room_version_rules::RoomVersionRules};
use tuwunel_core::{Err, Result, implement, result::NotFound};
use tuwunel_matrix::{Event, PduEvent};

/// A first remote join has no local create yet. A missing or unreadable
/// create in an existing snapshot is a storage error, not a first join.
#[implement(super::Service)]
pub(crate) async fn known_create_id(&self, room_id: &RoomId) -> Result<Option<OwnedEventId>> {
	let Some(hash) = self.services.state.get_room_shortstatehash(room_id).await.present()? else {
		return Ok(None);
	};
	self.services.state_accessor.state_get_id(hash, &StateEventType::RoomCreate, "").await.map(Some)
}

#[implement(super::Service)]
pub(crate) fn check_event_provenance(&self, pdu: &PduEvent, rules: &RoomVersionRules, create: Option<&EventId>) -> Result {
	if let Some(create) = create {
		check_create_reference(pdu, rules, create)?;
	}
	Ok(())
}

/// In earlier room versions every non-create must explicitly refer to the
/// create. In v12 the room ID itself supplies the reference instead.
fn check_create_reference(pdu: &PduEvent, rules: &RoomVersionRules, create: &EventId) -> Result {
	if pdu.kind == TimelineEventType::RoomCreate {
		if pdu.event_id() != create {
			return Err!(Request(Forbidden("The room already has a different create event.")));
		}
	} else if !rules.authorization.room_create_event_id_as_room_id && !pdu.auth_events().any(|id| id == create) {
		return Err!(Request(Forbidden("The event does not refer to the room's create event.")));
	}
	Ok(())
}

#[cfg(test)]
mod tests;
