use ruma::{
	CanonicalJsonMemberOptional as JsonMember, CanonicalJsonMembersOptional as JsonMembers,
	events::{
		AnyMessageLikeEvent, AnyStateEvent, AnyStrippedStateEvent, AnySyncMessageLikeEvent,
		AnySyncStateEvent, AnySyncTimelineEvent, AnyTimelineEvent, StateEvent, TimelineEventType,
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
			("unsigned", event.unsigned().map(Into::into)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
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
			("unsigned", event.unsigned().map(Into::into)),
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
		// MSC4311: the create event keeps origin_server_ts in stripped state.
		let create = matches!(event.event_type(), TimelineEventType::RoomCreate);
		let members: [JsonMember<_>; _] = [
			("content", Some(event.content().into())),
			("origin_server_ts", create.then(|| event.origin_server_ts().get().into())),
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
