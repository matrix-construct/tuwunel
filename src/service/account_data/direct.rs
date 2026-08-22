use ruma::{
	RoomId, UserId,
	events::{
		GlobalAccountDataEventType,
		direct::{DirectEvent, DirectEventContent, DirectUserIdentifier},
	},
};
use tuwunel_core::{Result, at, implement, is_equal_to};

#[implement(super::Service)]
pub async fn is_direct(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	self.services
		.account_data
		.get_global(user_id, GlobalAccountDataEventType::Direct)
		.await
		.map(|content: DirectEventContent| content)
		.into_iter()
		.flat_map(DirectEventContent::into_iter)
		.map(at!(1))
		.flat_map(Vec::into_iter)
		.any(is_equal_to!(room_id))
}

/// Record a room as a direct chat with `target` in the user's `m.direct`.
///
/// The room joins the counterparty's list, which is created when this is
/// their first direct room. A list already naming the room is left alone,
/// so repeated calls settle on one entry.
#[implement(super::Service)]
pub async fn mark_direct(&self, user_id: &UserId, target: &UserId, room_id: &RoomId) -> Result {
	let mut content = self
		.services
		.account_data
		.get_global(user_id, GlobalAccountDataEventType::Direct)
		.await
		.map(|event: DirectEvent| event.content)
		.unwrap_or_default();

	let target: &DirectUserIdentifier = target.into();
	let listed = content
		.get(target)
		.is_some_and(|rooms| rooms.iter().any(is_equal_to!(room_id)));

	if listed {
		return Ok(());
	}

	content
		.entry(target.to_owned())
		.or_default()
		.push(room_id.to_owned());

	let event = serde_json::to_value(DirectEvent { content })?;

	let event_type = GlobalAccountDataEventType::Direct
		.to_string()
		.into();

	self.services
		.account_data
		.update(None, user_id, event_type, &event)
		.await
}
