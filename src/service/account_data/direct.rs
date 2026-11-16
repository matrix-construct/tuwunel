//! Direct-room account data helpers.
//!
//! These operations read and update a user's `m.direct` mapping. Callers can list direct rooms
//! without selecting a counterparty.

use std::collections::BTreeSet;

use ruma::{
	OwnedRoomId, RoomId, UserId,
	events::{
		GlobalAccountDataEventType,
		direct::{DirectEvent, DirectEventContent, DirectUserIdentifier},
	},
};
use tuwunel_core::{Result, at, implement, is_equal_to};

/// The rooms the user's `m.direct` names, whoever the counterparty is.
///
/// The map is keyed by counterparty rather than by room, so answering for one
/// room means flattening all of it. A user with no `m.direct` at all has no
/// direct rooms.
#[implement(super::Service)]
pub async fn direct_rooms(&self, user_id: &UserId) -> BTreeSet<OwnedRoomId> {
	self.direct_content(user_id)
		.await
		.into_iter()
		.flat_map(DirectEventContent::into_iter)
		.map(at!(1))
		.flat_map(Vec::into_iter)
		.collect()
}

/// Record a room as a direct chat with `target` in the user's `m.direct`.
///
/// The room joins the counterparty's list, which is created when this is
/// their first direct room. A list already naming the room is left alone,
/// so repeated calls settle on one entry. The mapping is merged under the
/// user's update lock, and one that cannot be read fails the call rather than
/// being replaced.
///
/// # Panics
///
/// Panics when storing a changed mapping requires a global sequence number that cannot be allocated
/// or persisted.
#[implement(super::Service)]
pub async fn mark_direct(&self, user_id: &UserId, target: &UserId, room_id: &RoomId) -> Result {
	let event_type = GlobalAccountDataEventType::Direct
		.to_string()
		.into();

	self.modify(None, user_id, event_type, |event: Result<Option<DirectEvent>>| {
		let content = event?
			.map(|event| event.content)
			.unwrap_or_default();

		let event = with_direct_room(content, target.into(), room_id)
			.map(|content| DirectEvent { content });

		Ok(event)
	})
	.await
}

/// The user's `m.direct` content, absent when they have never had one.
///
/// Account data is stored as the whole event rather than as its content, so
/// the record deserializes as an event and the content comes off it.
#[implement(super::Service)]
async fn direct_content(&self, user_id: &UserId) -> Result<DirectEventContent> {
	self.services
		.account_data
		.get_global(user_id, GlobalAccountDataEventType::Direct)
		.await
		.map(|event: DirectEvent| event.content)
}

/// The mapping with the room in `target`'s list, absent when it is already there.
///
/// A counterparty with no list yet gets one holding just this room, and an
/// unchanged mapping stores no new revision.
fn with_direct_room(
	mut content: DirectEventContent,
	target: &DirectUserIdentifier,
	room_id: &RoomId,
) -> Option<DirectEventContent> {
	let rooms = content.entry(target.to_owned()).or_default();

	if rooms.iter().any(is_equal_to!(room_id)) {
		return None;
	}

	rooms.push(room_id.to_owned());

	Some(content)
}
