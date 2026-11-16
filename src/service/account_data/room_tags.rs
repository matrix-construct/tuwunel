//! Room tag account data helpers.
//!
//! The module updates and retrieves a user's `m.tag` content for an individual room. Tag writes
//! merge into the stored event under the user's update lock, so a tag saved meanwhile is kept, and
//! they edit its JSON rather than a typed copy, so fields the server does not model survive. A
//! stored event whose content or tag set is not an object is refused rather than replaced.

use futures::TryFutureExt;
use ruma::{
	RoomId, UserId,
	events::{
		RoomAccountDataEventType,
		tag::{TagEvent, TagInfo, TagName, Tags},
	},
};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use tuwunel_core::{Result, err, implement};

/// Add a tag to the room in the user's account data.
///
/// The stored event is merged under the user's update lock, so concurrent tag writes all land, and
/// only the named tag changes, so other tags and fields the server does not model are kept. Given
/// `info` replaces a tag already present; without it, a present tag keeps its own and nothing is
/// written. A stored event whose tags cannot be read fails the call rather than being replaced.
///
/// # Panics
///
/// Panics when dispatching the global sequence number for the changed tag set fails.
#[implement(super::Service)]
pub async fn set_room_tag(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	tag: TagName,
	info: Option<TagInfo>,
) -> Result {
	self.modify(Some(room_id), user_id, RoomAccountDataEventType::Tag, |event| {
		with_tag(event?.unwrap_or_else(untagged), &tag, info)
	})
	.await
}

/// Remove a tag from the room in the user's account data.
///
/// The stored event is merged under the user's update lock as [`Self::set_room_tag`] merges it,
/// and nothing is written when the tag is not among it.
///
/// # Panics
///
/// Panics when dispatching the global sequence number for the changed tag set fails.
#[implement(super::Service)]
pub async fn remove_room_tag(&self, user_id: &UserId, room_id: &RoomId, tag: &TagName) -> Result {
	self.modify(Some(room_id), user_id, RoomAccountDataEventType::Tag, |event| {
		event?.map_or(Ok(None), |event| without_tag(event, tag))
	})
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

fn untagged() -> JsonValue { json!({ "type": RoomAccountDataEventType::Tag, "content": {} }) }

/// The event with `tag` set, absent when nothing changes.
///
/// Given `info` replaces the tag's own; without it, a tag already present is
/// kept as it is.
fn with_tag(
	mut event: JsonValue,
	tag: &TagName,
	info: Option<TagInfo>,
) -> Result<Option<JsonValue>> {
	let tags = tags_mut(&mut event)?;

	if info.is_none() && tags.contains_key(tag.as_ref()) {
		return Ok(None);
	}

	tags.insert(tag.as_ref().to_owned(), serde_json::to_value(info.unwrap_or_default())?);

	Ok(Some(event))
}

/// The event without `tag`, absent when it is not among the event's tags.
///
/// Removing a tag the room does not carry then stores no new revision for
/// sync to deliver.
fn without_tag(mut event: JsonValue, tag: &TagName) -> Result<Option<JsonValue>> {
	let removed = tags_mut(&mut event)?.remove(tag.as_ref());

	Ok(removed.map(|_| event))
}

/// The event's tag set, created empty when its content has none.
///
/// Content that is not an object, or a tag set that is not one, cannot be
/// merged into, so the write fails rather than replace what it cannot read.
fn tags_mut(event: &mut JsonValue) -> Result<&mut JsonMap<String, JsonValue>> {
	event
		.get_mut("content")
		.and_then(JsonValue::as_object_mut)
		.map(|content| content.entry("tags").or_insert_with(|| json!({})))
		.and_then(JsonValue::as_object_mut)
		.ok_or_else(|| err!(Database("Stored room tags are not a JSON object.")))
}
