use std::{
	borrow::Borrow,
	fmt::{Debug, Display},
	hash::Hash,
};

use ruma::{EventId, MilliSecondsSinceUnixEpoch, RoomId, UserId, events::TimelineEventType};
use serde_json::value::RawValue as RawJsonValue;

/// Abstraction of a PDU so users can have their own PDU types.
pub trait Event {
	type Id: Clone + Debug + Display + Eq + Ord + Hash + Send + Borrow<EventId>;

	/// The `EventId` of this event.
	fn event_id(&self) -> &Self::Id;

	/// The `RoomId` of this event.
	fn room_id(&self) -> &RoomId;

	/// The `UserId` of this event.
	fn sender(&self) -> &UserId;

	/// The time of creation on the originating server.
	fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch;

	/// The event type.
	fn event_type(&self) -> &TimelineEventType;

	/// The event's content.
	fn content(&self) -> &RawJsonValue;

	/// The state key for this event.
	fn state_key(&self) -> Option<&str>;

	/// The events before this event.
	// Requires GATs to avoid boxing (and TAIT for making it convenient).
	fn prev_events(&self) -> impl DoubleEndedIterator<Item = &Self::Id> + Send + '_;

	/// All the authenticating events for this event.
	// Requires GATs to avoid boxing (and TAIT for making it convenient).
	fn auth_events(&self) -> impl DoubleEndedIterator<Item = &Self::Id> + Send + '_;

	/// If this event is a redaction event this is the event it redacts.
	fn redacts(&self) -> Option<&Self::Id>;
}

impl<T: Event> Event for &T {
	type Id = T::Id;

	fn event_id(&self) -> &Self::Id {
		(*self).event_id()
	}

	fn room_id(&self) -> &RoomId {
		(*self).room_id()
	}

	fn sender(&self) -> &UserId {
		(*self).sender()
	}

	fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
		(*self).origin_server_ts()
	}

	fn event_type(&self) -> &TimelineEventType {
		(*self).event_type()
	}

	fn content(&self) -> &RawJsonValue {
		(*self).content()
	}

	fn state_key(&self) -> Option<&str> {
		(*self).state_key()
	}

	fn prev_events(&self) -> impl DoubleEndedIterator<Item = &Self::Id> + Send + '_ {
		(*self).prev_events()
	}

	fn auth_events(&self) -> impl DoubleEndedIterator<Item = &Self::Id> + Send + '_ {
		(*self).auth_events()
	}

	fn redacts(&self) -> Option<&Self::Id> {
		(*self).redacts()
	}
}
