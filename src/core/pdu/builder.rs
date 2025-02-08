use std::collections::BTreeMap;

use ruma::{
	events::{EventContent, MessageLikeEventType, StateEventType, TimelineEventType},
	MilliSecondsSinceUnixEpoch, OwnedEventId,
};
use serde::Deserialize;
use serde_json::value::{to_raw_value, RawValue as RawJsonValue};

use super::StateKey;

/// Build the start of a PDU in order to add it to the Database.
#[derive(Debug, Deserialize)]
pub struct Builder {
	#[serde(rename = "type")]
	pub event_type: TimelineEventType,

	pub content: Box<RawJsonValue>,

	pub unsigned: Option<Unsigned>,

	pub state_key: Option<StateKey>,

	pub redacts: Option<OwnedEventId>,

	/// For timestamped messaging, should only be used for appservices.
	/// Will be set to current time if None
	pub timestamp: Option<MilliSecondsSinceUnixEpoch>,
}

type Unsigned = BTreeMap<String, serde_json::Value>;

impl Builder {
	pub fn state<S, T>(state_key: S, content: &T) -> Self
	where
		T: EventContent<EventType = StateEventType>,
		S: Into<StateKey>,
	{
		Self {
			event_type: content.event_type().into(),
			content: to_raw_value(content)
				.expect("Builder failed to serialize state event content to RawValue"),
			state_key: Some(state_key.into()),
			..Self::default()
		}
	}

	pub fn timeline<T>(content: &T) -> Self
	where
		T: EventContent<EventType = MessageLikeEventType>,
	{
		Self {
			event_type: content.event_type().into(),
			content: to_raw_value(content)
				.expect("Builder failed to serialize timeline event content to RawValue"),
			..Self::default()
		}
	}
}

impl Default for Builder {
	fn default() -> Self {
		Self {
			event_type: "m.room.message".into(),
			content: Box::<RawJsonValue>::default(),
			unsigned: None,
			state_key: None,
			redacts: None,
			timestamp: None,
		}
	}
}
