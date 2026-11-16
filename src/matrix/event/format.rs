use ruma::{
	CanonicalJsonMemberOptional as JsonMember, CanonicalJsonMembersOptional as JsonMembers,
	CanonicalJsonValue, RoomId,
	events::{
		AnyMessageLikeEvent, AnyStateEvent, AnyStrippedStateEvent, AnySyncMessageLikeEvent,
		AnySyncStateEvent, AnySyncTimelineEvent, AnyTimelineEvent, StateEvent,
		room::member::RoomMemberEventContent, space::child::HierarchySpaceChildEvent,
	},
	serde::Raw,
};
use serde_json::value::to_raw_value;

use super::{Event, redact::copy};

/// Owns an event for conversion into a Ruma event envelope.
///
/// `From` implementations select the fields required by each client event
/// shape. Consuming the wrapper also consumes the source event.
pub struct Owned<E: Event>(pub(super) E);

/// Borrows an event for conversion into a Ruma event envelope.
///
/// `From` implementations select the fields required by each client event
/// shape. The source event remains available after conversion.
pub struct Ref<'a, E: Event>(pub(super) &'a E);

/// Paths in `unsigned` to the bundled events a room format names the room on.
///
/// A bundled event is served in the form of the event that bundles it
/// (MSC3440), and only the room formats carry a `room_id`. The stored copies
/// hold the sync form, which has none.
const ROOM_BUNDLES: &[&[&str]] = &[&["m.relations", "m.thread", "latest_event"]];

pub(super) fn to_sync_message_like_without_unsigned<E: Event>(
	event: &E,
) -> Raw<AnySyncMessageLikeEvent> {
	let (redacts, content) = copy(event);
	let event_type = event.event_type().to_cow_str();
	let members: [JsonMember<_>; _] = [
		("content", Some(content.json().into())),
		("event_id", Some(event.event_id().as_str().into())),
		("origin_server_ts", Some(event.origin_server_ts().get().into())),
		("redacts", redacts.map(|e| e.as_str().into())),
		("sender", Some(event.sender().as_str().into())),
		("state_key", event.state_key().map(Into::into)),
		("type", Some(event_type.as_ref().into())),
		("unsigned", None),
	];

	to_raw_value(&JsonMembers(&members))
		.map(Raw::from_json)
		.expect("Failed to serialize Event value")
}

impl<E: Event> From<Owned<E>> for Raw<AnySyncTimelineEvent> {
	fn from(event: Owned<E>) -> Self { Ref(&event.0).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnySyncTimelineEvent> {
	fn from(event: Ref<'a, E>) -> Self {
		let event = event.0;
		let (redacts, content) = copy(event);
		let members: [JsonMember<_>; _] = [
			("content", Some(content.json().into())),
			("event_id", Some(event.event_id().as_str().into())),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("redacts", redacts.map(|e| e.as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", event.unsigned().map(Into::into)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

impl<E: Event> From<Owned<E>> for Raw<AnyTimelineEvent> {
	fn from(event: Owned<E>) -> Self { Ref(&event.0).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnyTimelineEvent> {
	fn from(event: Ref<'a, E>) -> Self {
		let event = event.0;
		let (redacts, content) = copy(event);
		let members: [JsonMember<_>; _] = [
			("content", Some(content.json().into())),
			("event_id", Some(event.event_id().as_str().into())),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("redacts", redacts.map(|e| e.as_str().into())),
			("room_id", Some(event.room_id().as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", room_unsigned(event)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

/// Copies the event's `unsigned` for a room format, naming the room on each
/// bundled event.
///
/// A bundled event shares the room of the event bundling it, and one that
/// already names a room keeps it.
fn room_unsigned<E: Event>(event: &E) -> Option<CanonicalJsonValue> {
	let room_id = event.room_id();

	event
		.unsigned()
		.map(CanonicalJsonValue::from)
		.map(|unsigned| {
			ROOM_BUNDLES
				.iter()
				.fold(unsigned, |unsigned, path| name_room(unsigned, path, room_id))
		})
}

fn name_room(
	mut unsigned: CanonicalJsonValue,
	path: &[&str],
	room_id: &RoomId,
) -> CanonicalJsonValue {
	let bundled = path
		.iter()
		.try_fold(&mut unsigned, |value, field| value.as_object_mut()?.get_mut(*field));

	if let Some(CanonicalJsonValue::Object(event)) = bundled {
		event
			.entry("room_id".into())
			.or_insert_with(|| room_id.as_str().into());
	}

	unsigned
}

impl<E: Event> From<Owned<E>> for Raw<AnyMessageLikeEvent> {
	fn from(event: Owned<E>) -> Self { Ref(&event.0).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnyMessageLikeEvent> {
	fn from(event: Ref<'a, E>) -> Self {
		let event = event.0;
		let (redacts, content) = copy(event);
		let members: [JsonMember<_>; _] = [
			("content", Some(content.json().into())),
			("event_id", Some(event.event_id().as_str().into())),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("redacts", redacts.map(|e| e.as_str().into())),
			("room_id", Some(event.room_id().as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", room_unsigned(event)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

impl<E: Event> From<Owned<E>> for Raw<AnySyncMessageLikeEvent> {
	fn from(event: Owned<E>) -> Self { Ref(&event.0).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnySyncMessageLikeEvent> {
	fn from(event: Ref<'a, E>) -> Self {
		let event = event.0;
		let (redacts, content) = copy(event);
		let members: [JsonMember<_>; _] = [
			("content", Some(content.json().into())),
			("event_id", Some(event.event_id().as_str().into())),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("redacts", redacts.map(|e| e.as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", event.unsigned().map(Into::into)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

impl<E: Event> From<Owned<E>> for Raw<AnyStateEvent> {
	fn from(event: Owned<E>) -> Self { Ref(&event.0).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnyStateEvent> {
	fn from(event: Ref<'a, E>) -> Self {
		let event = event.0;
		let members: [JsonMember<_>; _] = [
			("content", Some(event.content().into())),
			("event_id", Some(event.event_id().as_str().into())),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("room_id", Some(event.room_id().as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", event.unsigned().map(Into::into)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

impl<E: Event> From<Owned<E>> for Raw<AnySyncStateEvent> {
	fn from(event: Owned<E>) -> Self { Ref(&event.0).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnySyncStateEvent> {
	fn from(event: Ref<'a, E>) -> Self {
		let event = event.0;
		let members: [JsonMember<_>; _] = [
			("content", Some(event.content().into())),
			("event_id", Some(event.event_id().as_str().into())),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", event.unsigned().map(Into::into)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

impl<E: Event> From<Owned<E>> for Raw<AnyStrippedStateEvent> {
	fn from(event: Owned<E>) -> Self { Ref(&event.0).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnyStrippedStateEvent> {
	fn from(event: Ref<'a, E>) -> Self {
		let event = event.0;
		let members: [JsonMember<_>; _] = [
			("content", Some(event.content().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

impl<E: Event> From<Owned<E>> for Raw<HierarchySpaceChildEvent> {
	fn from(event: Owned<E>) -> Self { Ref(&event.0).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<HierarchySpaceChildEvent> {
	fn from(event: Ref<'a, E>) -> Self {
		let event = event.0;
		let members: [JsonMember<_>; _] = [
			("content", Some(event.content().into())),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

impl<E: Event> From<Owned<E>> for Raw<StateEvent<RoomMemberEventContent>> {
	fn from(event: Owned<E>) -> Self { Ref(&event.0).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<StateEvent<RoomMemberEventContent>> {
	fn from(event: Ref<'a, E>) -> Self {
		let event = event.0;
		let members: [JsonMember<_>; _] = [
			("content", Some(event.content().into())),
			("event_id", Some(event.event_id().as_str().into())),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("redacts", event.redacts().map(|e| e.as_str().into())),
			("room_id", Some(event.room_id().as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", event.unsigned().map(Into::into)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}
