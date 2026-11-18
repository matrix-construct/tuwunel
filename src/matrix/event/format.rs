use ruma::{
	CanonicalJsonMemberOptional as JsonMember, CanonicalJsonMembersOptional as JsonMembers,
	CanonicalJsonObject, CanonicalJsonValue, Int, RoomId,
	events::{
		AnyMessageLikeEvent, AnyStateEvent, AnyStrippedStateEvent, AnySyncMessageLikeEvent,
		AnySyncStateEvent, AnySyncTimelineEvent, AnyTimelineEvent, StateEvent,
		room::member::RoomMemberEventContent, space::child::HierarchySpaceChildEvent,
	},
	serde::Raw,
};
use serde_json::value::to_raw_value;
use tuwunel_core::utils::{BoolExt, time::now_millis};

use super::{
	Event,
	redact::copy,
	sticky::{STICKY_KEY, STICKY_TTL_KEY, nested_remaining},
};

/// Owns an event for conversion into a Ruma event envelope.
///
/// `From` implementations select the fields required by each client event
/// shape. Consuming the wrapper also consumes the source event. The optional
/// instant judges MSC4354 stickiness; without one a sticky event reads the
/// clock.
pub struct Owned<E: Event>(pub(super) E, pub(super) Option<u64>);

/// Borrows an event for conversion into a Ruma event envelope.
///
/// `From` implementations select the fields required by each client event
/// shape. The source event remains available after conversion. The optional
/// instant judges MSC4354 stickiness; without one a sticky event reads the
/// clock.
pub struct Ref<'a, E: Event>(pub(super) &'a E, pub(super) Option<u64>);

/// Paths in `unsigned` to the bundled events a room format names the room on.
///
/// A bundled event is served in the form of the event that bundles it
/// (MSC3440), and only the room formats carry a `room_id`. The stored copies
/// hold the sync form, which has none.
const ROOM_BUNDLES: &[&[&str]] = &[&["m.relations", "m.thread", "latest_event"]];

/// Paths in `unsigned` to the event copies served with their own remaining
/// sticky time (MSC4354).
///
/// Both are stored without `unsigned`, so the time is added as they are
/// served.
const NESTED_EVENTS: &[&[&str]] =
	&[&["m.relations", "m.thread", "latest_event"], &["redacted_because"]];

pub(super) fn to_sync_message_like_without_unsigned<E: Event>(
	event: &E,
) -> Raw<AnySyncMessageLikeEvent> {
	let (redacts, content) = copy(event);
	let event_type = event.event_type().to_cow_str();
	let members: [JsonMember<_>; _] = [
		("content", Some(content.json().into())),
		("event_id", Some(event.event_id().as_str().into())),
		(STICKY_KEY, sticky_object(event)),
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

fn sticky_object<E: Event>(event: &E) -> Option<CanonicalJsonValue> {
	event
		.sticky()
		.and_then(|sticky| Int::try_from(sticky.duration_ms).ok())
		.map(|duration| {
			let duration = CanonicalJsonValue::Integer(duration);

			CanonicalJsonValue::Object([("duration_ms".into(), duration)].into())
		})
}

impl<E: Event> From<Owned<E>> for Raw<AnySyncTimelineEvent> {
	fn from(Owned(event, now): Owned<E>) -> Self { Ref(&event, now).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnySyncTimelineEvent> {
	fn from(Ref(event, now): Ref<'a, E>) -> Self {
		let (redacts, content) = copy(event);
		let members: [JsonMember<_>; _] = [
			("content", Some(content.json().into())),
			("event_id", Some(event.event_id().as_str().into())),
			(STICKY_KEY, sticky_object(event)),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("redacts", redacts.map(|e| e.as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", sync_unsigned(event, now)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

fn sync_unsigned<E: Event>(event: &E, now: Option<u64>) -> Option<CanonicalJsonValue> {
	let unsigned = event.unsigned().map(CanonicalJsonValue::from);

	sticky_unsigned(unsigned, event, now)
}

/// Sets MSC4354's remaining sticky time in a copy of `unsigned`, for the event
/// and for each event copy nested there.
///
/// A copy that is not sticky at the instant keeps no remaining time, even one
/// written when it was formatted earlier. Without an instant only a sticky
/// event reads the clock, so formatting any other event costs nothing extra.
fn sticky_unsigned<E: Event>(
	unsigned: Option<CanonicalJsonValue>,
	event: &E,
	now: Option<u64>,
) -> Option<CanonicalJsonValue> {
	let clock = || now.unwrap_or_else(now_millis);
	let unsigned = unsigned.map(|unsigned| {
		NESTED_EVENTS
			.iter()
			.fold(unsigned, |unsigned, path| nested_ttl(unsigned, path, clock))
	});

	let ttl = event
		.sticky()
		.and_then(|_| event.sticky_remaining(clock()));

	with_ttl(unsigned, ttl)
}

/// Sets the remaining sticky time of the event copy at `path`.
///
/// The copy was stored without its `unsigned`, so the time is computed from
/// its own timestamp and sticky object at format time.
fn nested_ttl<C>(mut unsigned: CanonicalJsonValue, path: &[&str], clock: C) -> CanonicalJsonValue
where
	C: Fn() -> u64,
{
	let nested = path
		.iter()
		.try_fold(&mut unsigned, |value, field| value.as_object_mut()?.get_mut(*field));

	if let Some(CanonicalJsonValue::Object(event)) = nested {
		let ttl = event
			.contains_key(STICKY_KEY)
			.and_then(|| nested_remaining(event, clock()));

		if let Some(nested_unsigned) = with_ttl(event.remove("unsigned"), ttl) {
			event.insert("unsigned".into(), nested_unsigned);
		}
	}

	unsigned
}

fn with_ttl(
	unsigned: Option<CanonicalJsonValue>,
	ttl: Option<u64>,
) -> Option<CanonicalJsonValue> {
	match ttl {
		| None => unsigned.map(remove_ttl),
		| Some(ttl) => Some(insert_ttl(unsigned.unwrap_or_else(empty_object), ttl)),
	}
}

fn insert_ttl(mut unsigned: CanonicalJsonValue, ttl: u64) -> CanonicalJsonValue {
	if let CanonicalJsonValue::Object(object) = &mut unsigned
		&& let Ok(ttl) = Int::try_from(ttl)
	{
		object.insert(STICKY_TTL_KEY.into(), CanonicalJsonValue::Integer(ttl));
	}

	unsigned
}

fn remove_ttl(mut unsigned: CanonicalJsonValue) -> CanonicalJsonValue {
	if let CanonicalJsonValue::Object(object) = &mut unsigned {
		object.remove(STICKY_TTL_KEY);
	}

	unsigned
}

fn empty_object() -> CanonicalJsonValue { CanonicalJsonValue::Object(CanonicalJsonObject::new()) }

impl<E: Event> From<Owned<E>> for Raw<AnyTimelineEvent> {
	fn from(Owned(event, now): Owned<E>) -> Self { Ref(&event, now).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnyTimelineEvent> {
	fn from(Ref(event, now): Ref<'a, E>) -> Self {
		let (redacts, content) = copy(event);
		let members: [JsonMember<_>; _] = [
			("content", Some(content.json().into())),
			("event_id", Some(event.event_id().as_str().into())),
			(STICKY_KEY, sticky_object(event)),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("redacts", redacts.map(|e| e.as_str().into())),
			("room_id", Some(event.room_id().as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", room_unsigned(event, now)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

/// Copies the event's `unsigned` for a room format, naming the room on each
/// bundled event and adding the remaining sticky time.
///
/// A bundled event shares the room of the event bundling it, and one that
/// already names a room keeps it.
fn room_unsigned<E: Event>(event: &E, now: Option<u64>) -> Option<CanonicalJsonValue> {
	let room_id = event.room_id();
	let unsigned = event
		.unsigned()
		.map(CanonicalJsonValue::from)
		.map(|unsigned| {
			ROOM_BUNDLES
				.iter()
				.fold(unsigned, |unsigned, path| name_room(unsigned, path, room_id))
		});

	sticky_unsigned(unsigned, event, now)
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
	fn from(Owned(event, now): Owned<E>) -> Self { Ref(&event, now).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnyMessageLikeEvent> {
	fn from(Ref(event, now): Ref<'a, E>) -> Self {
		let (redacts, content) = copy(event);
		let members: [JsonMember<_>; _] = [
			("content", Some(content.json().into())),
			("event_id", Some(event.event_id().as_str().into())),
			(STICKY_KEY, sticky_object(event)),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("redacts", redacts.map(|e| e.as_str().into())),
			("room_id", Some(event.room_id().as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", room_unsigned(event, now)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

impl<E: Event> From<Owned<E>> for Raw<AnySyncMessageLikeEvent> {
	fn from(Owned(event, now): Owned<E>) -> Self { Ref(&event, now).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnySyncMessageLikeEvent> {
	fn from(Ref(event, now): Ref<'a, E>) -> Self {
		let (redacts, content) = copy(event);
		let members: [JsonMember<_>; _] = [
			("content", Some(content.json().into())),
			("event_id", Some(event.event_id().as_str().into())),
			(STICKY_KEY, sticky_object(event)),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("redacts", redacts.map(|e| e.as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", sync_unsigned(event, now)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

impl<E: Event> From<Owned<E>> for Raw<AnyStateEvent> {
	fn from(Owned(event, now): Owned<E>) -> Self { Ref(&event, now).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnyStateEvent> {
	fn from(Ref(event, now): Ref<'a, E>) -> Self {
		let members: [JsonMember<_>; _] = [
			("content", Some(event.content().into())),
			("event_id", Some(event.event_id().as_str().into())),
			(STICKY_KEY, sticky_object(event)),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("room_id", Some(event.room_id().as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", sync_unsigned(event, now)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

impl<E: Event> From<Owned<E>> for Raw<AnySyncStateEvent> {
	fn from(Owned(event, now): Owned<E>) -> Self { Ref(&event, now).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnySyncStateEvent> {
	fn from(Ref(event, now): Ref<'a, E>) -> Self {
		let members: [JsonMember<_>; _] = [
			("content", Some(event.content().into())),
			("event_id", Some(event.event_id().as_str().into())),
			(STICKY_KEY, sticky_object(event)),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", sync_unsigned(event, now)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}

impl<E: Event> From<Owned<E>> for Raw<AnyStrippedStateEvent> {
	fn from(Owned(event, now): Owned<E>) -> Self { Ref(&event, now).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<AnyStrippedStateEvent> {
	fn from(Ref(event, _): Ref<'a, E>) -> Self {
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
	fn from(Owned(event, now): Owned<E>) -> Self { Ref(&event, now).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<HierarchySpaceChildEvent> {
	fn from(Ref(event, _): Ref<'a, E>) -> Self {
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
	fn from(Owned(event, now): Owned<E>) -> Self { Ref(&event, now).into() }
}

impl<'a, E: Event> From<Ref<'a, E>> for Raw<StateEvent<RoomMemberEventContent>> {
	fn from(Ref(event, now): Ref<'a, E>) -> Self {
		let members: [JsonMember<_>; _] = [
			("content", Some(event.content().into())),
			("event_id", Some(event.event_id().as_str().into())),
			(STICKY_KEY, sticky_object(event)),
			("origin_server_ts", Some(event.origin_server_ts().get().into())),
			("redacts", event.redacts().map(|e| e.as_str().into())),
			("room_id", Some(event.room_id().as_str().into())),
			("sender", Some(event.sender().as_str().into())),
			("state_key", event.state_key().map(Into::into)),
			("type", Some(event.event_type().to_string().into())),
			("unsigned", sync_unsigned(event, now)),
		];

		to_raw_value(&JsonMembers(&members))
			.map(Self::from_json)
			.expect("Failed to serialize Event value")
	}
}
