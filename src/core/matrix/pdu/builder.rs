use std::{collections::BTreeMap, fmt};

use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedEventId,
	events::{MessageLikeEventContent, StateEventContent, TimelineEventType},
};
use serde::Deserialize;
use serde_json::value::{RawValue as RawJsonValue, to_raw_value};

use super::{Content, StateKey};

/// Collects the initial fields needed to build and append a PDU.
///
/// The timeline service supplies event graph fields, sender data, and hashes
/// after this value is created. Constructors serialize typed Ruma event
/// content.
#[derive(Deserialize)]
pub struct Builder {
	/// Matrix event type for the new PDU.
	///
	/// Typed constructors derive this value from the supplied event content.
	#[serde(rename = "type")]
	pub event_type: TimelineEventType,

	/// Raw canonical content for the new event.
	///
	/// The builder retains encoded content until the PDU is assembled and
	/// signed.
	pub content: Content,

	/// Unsigned metadata to include with the new event.
	///
	/// Typical values include local transaction IDs and prior-state metadata.
	pub unsigned: Option<BTreeMap<String, serde_json::Value>>,

	/// State key for a state event.
	///
	/// A missing key identifies a message-like timeline event.
	pub state_key: Option<StateKey>,

	/// Event ID targeted by a redaction.
	///
	/// The value becomes the legacy top-level `redacts` field. Callers place a
	/// content-based redaction target in the encoded content separately.
	pub redacts: Option<OwnedEventId>,

	/// Overrides the event timestamp for appservice messaging.
	///
	/// An absent value uses the current time when the PDU is built. Ordinary
	/// callers should leave this field unset.
	pub timestamp: Option<MilliSecondsSinceUnixEpoch>,
}

impl Default for Builder {
	fn default() -> Self {
		Self {
			event_type: "m.room.message".into(),
			content: Box::<RawJsonValue>::default().into(),
			unsigned: None,
			state_key: None,
			redacts: None,
			timestamp: None,
		}
	}
}

impl Builder {
	/// Builds a state-event template from typed event content.
	///
	/// The content's event type and supplied state key populate the
	/// corresponding builder fields. Remaining optional fields use their
	/// defaults.
	///
	/// # Panics
	///
	/// Panics if the event content cannot be serialized as raw JSON.
	pub fn state<S, T>(state_key: S, content: &T) -> Self
	where
		T: StateEventContent,
		S: Into<StateKey>,
	{
		Self {
			event_type: content.event_type().into(),
			content: to_raw_value(content)
				.map(Into::into)
				.expect("Builder failed to serialize state event content to RawValue"),
			state_key: Some(state_key.into()),
			..Self::default()
		}
	}

	/// Builds a message-like event template from typed event content.
	///
	/// The content's event type populates the builder and the state key remains
	/// absent. Remaining optional fields use their defaults.
	///
	/// # Panics
	///
	/// Panics if the event content cannot be serialized as raw JSON.
	pub fn timeline<T>(content: &T) -> Self
	where
		T: MessageLikeEventContent,
	{
		Self {
			event_type: content.event_type().into(),
			content: to_raw_value(content)
				.map(Into::into)
				.expect("Builder failed to serialize timeline event content to RawValue"),
			..Self::default()
		}
	}
}

impl fmt::Debug for Builder {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let mut d = f.debug_struct("Builder");

		d.field("type", &self.event_type);

		if let Some(state_key) = self.state_key.as_ref() {
			d.field("state_key", state_key);
		}

		if let Some(redacts) = self.redacts.as_ref() {
			d.field("redacts", redacts);
		}

		if let Some(timestamp) = self.timestamp.as_ref() {
			d.field("ts", timestamp);
		}

		if let Some(unsigned) = self.unsigned.as_ref() {
			d.field("unsigned", unsigned);
		}

		d.field("content", &self.content);

		d.finish()
	}
}
