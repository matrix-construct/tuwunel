use ruma::{
	EventId, OwnedEventId, RoomId, ServerName, UserId,
	events::{StateEventType, TimelineEventType, room::member::MembershipState},
	room_version_rules::RoomVersionRules,
};
use tuwunel_core::{Err, Result, implement, result::NotFound};
use tuwunel_matrix::{Event, PduEvent, room::state::resolution::events::RoomMemberEvent};

/// Returns the create event of the room's current state, or none before the
/// room has state.
///
/// A first remote join has no local create yet. A missing or unreadable
/// create in an existing snapshot is a storage error, not a first join.
#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip_all, fields(%room_id))]
pub(crate) async fn known_create_id(&self, room_id: &RoomId) -> Result<Option<OwnedEventId>> {
	let Some(hash) = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.await
		.present()?
	else {
		return Ok(None);
	};

	self.services
		.state_accessor
		.state_get_id(hash, &StateEventType::RoomCreate, "")
		.await
		.map(Some)
}

/// Checks that a local user's join was sent by that user, and that the event
/// names the room's known create.
///
/// An initial join has no stored create yet, but the local sender check still
/// applies.
#[implement(super::Service)]
pub(crate) fn check_event_provenance(
	&self,
	pdu: &PduEvent,
	rules: &RoomVersionRules,
	create: Option<&EventId>,
) -> Result {
	check_local_join_sender(pdu, self.services.globals.server_name())?;
	if let Some(create) = create {
		check_create_reference(pdu, rules, create)?;
	}

	Ok(())
}

/// Refuses a local user's join that another user sent.
///
/// This server sends its own users' joins itself. The check is local admission
/// policy, not a change to the room version's authorization rules.
fn check_local_join_sender(pdu: &PduEvent, local: &ServerName) -> Result {
	if pdu.kind == TimelineEventType::RoomMember
		&& let Some(key) = pdu.state_key()
		&& key != pdu.sender().as_str()
		&& <&UserId>::try_from(key).is_ok_and(|user| user.server_name() == local)
		&& RoomMemberEvent::new(pdu).membership()? == MembershipState::Join
	{
		return Err!(Request(Forbidden("A local user's join must be sent by that user.")));
	}

	Ok(())
}

/// Checks that a create event is the known one and that any other event refers
/// to it.
///
/// In earlier room versions every non-create must explicitly refer to the
/// create. In v12 the room ID itself supplies the reference instead.
fn check_create_reference(pdu: &PduEvent, rules: &RoomVersionRules, create: &EventId) -> Result {
	let implied = rules
		.authorization
		.room_create_event_id_as_room_id;

	match pdu.kind {
		| TimelineEventType::RoomCreate if pdu.event_id() == create => Ok(()),
		| TimelineEventType::RoomCreate =>
			Err!(Request(Forbidden("The room already has a different create event."))),
		| _ if implied || pdu.auth_events().any(|id| id == create) => Ok(()),
		| _ => Err!(Request(Forbidden("The event does not refer to the room's create event."))),
	}
}

#[cfg(test)]
mod tests;
