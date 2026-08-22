use futures::TryFutureExt;
use ruma::{
	RoomId, UserId,
	events::{
		RoomAccountDataEventType,
		tag::{TagEvent, TagEventContent, TagInfo, TagName, Tags},
	},
};
use tuwunel_core::{Result, implement};

/// Add a tag to the room in the user's account data.
///
/// The whole tag set is rewritten on every call, so it is read back first and
/// the new tag merged into it. A tag already present is replaced.
#[implement(super::Service)]
pub async fn set_room_tag(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	tag: TagName,
	info: Option<TagInfo>,
) -> Result {
	let mut tags = self
		.get_room_tags(user_id, room_id)
		.await
		.unwrap_or_default();

	tags.insert(tag, info.unwrap_or_default());

	let event = serde_json::to_value(TagEvent { content: TagEventContent { tags } })?;

	self.update(Some(room_id), user_id, RoomAccountDataEventType::Tag, &event)
		.await
}

/// The tags the user has placed on the room, absent when there are none.
///
/// Account data is stored as the whole event rather than as its content, so a
/// read naming the content type here fails on every record.
#[implement(super::Service)]
pub async fn get_room_tags(&self, user_id: &UserId, room_id: &RoomId) -> Result<Tags> {
	self.services
		.account_data
		.get_room(room_id, user_id, RoomAccountDataEventType::Tag)
		.map_ok(|event: TagEvent| event.content.tags)
		.await
}
