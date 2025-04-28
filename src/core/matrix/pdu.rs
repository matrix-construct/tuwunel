mod builder;
mod content;
mod count;
mod event_id;
mod filter;
mod id;
mod raw_id;
mod redact;
mod relation;
mod state_key;
mod strip;
#[cfg(test)]
mod tests;
mod unsigned;

use std::cmp::Ordering;

use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId,
	OwnedRoomId, OwnedServerName, OwnedUserId, RoomId, UInt, UserId, events::TimelineEventType,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue as RawJsonValue;

pub use self::{
	Count as PduCount, Id as PduId, Pdu as PduEvent, RawId as RawPduId,
	builder::{Builder, Builder as PduBuilder},
	count::Count,
	event_id::*,
	id::*,
	raw_id::*,
	state_key::{ShortStateKey, StateKey},
};
use super::Event;
use crate::Result;

/// Persistent Data Unit (Event)
#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct Pdu {
	pub event_id: OwnedEventId,
	pub room_id: OwnedRoomId,
	pub sender: OwnedUserId,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub origin: Option<OwnedServerName>,
	pub origin_server_ts: UInt,
	#[serde(rename = "type")]
	pub kind: TimelineEventType,
	pub content: Box<RawJsonValue>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub state_key: Option<StateKey>,
	pub prev_events: Vec<OwnedEventId>,
	pub depth: UInt,
	pub auth_events: Vec<OwnedEventId>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub redacts: Option<OwnedEventId>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub unsigned: Option<Box<RawJsonValue>>,
	pub hashes: EventHash,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	// BTreeMap<Box<ServerName>, BTreeMap<ServerSigningKeyId, String>>
	pub signatures: Option<Box<RawJsonValue>>,
}

/// Content hashes of a PDU.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventHash {
	/// The SHA-256 hash.
	pub sha256: String,
}

impl Pdu {
	pub fn from_id_val(event_id: &EventId, mut json: CanonicalJsonObject) -> Result<Self> {
		let event_id = CanonicalJsonValue::String(event_id.into());
		json.insert("event_id".into(), event_id);
		serde_json::to_value(json)
			.and_then(serde_json::from_value)
			.map_err(Into::into)
	}
}

impl Event for Pdu {
	type Id = OwnedEventId;

	fn event_id(&self) -> &Self::Id {
		&self.event_id
	}

	fn room_id(&self) -> &RoomId {
		&self.room_id
	}

	fn sender(&self) -> &UserId {
		&self.sender
	}

	fn event_type(&self) -> &TimelineEventType {
		&self.kind
	}

	fn content(&self) -> &RawJsonValue {
		&self.content
	}

	fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
		MilliSecondsSinceUnixEpoch(self.origin_server_ts)
	}

	fn state_key(&self) -> Option<&str> {
		self.state_key.as_deref()
	}

	fn prev_events(&self) -> impl DoubleEndedIterator<Item = &Self::Id> + Send + '_ {
		self.prev_events.iter()
	}

	fn auth_events(&self) -> impl DoubleEndedIterator<Item = &Self::Id> + Send + '_ {
		self.auth_events.iter()
	}

	fn redacts(&self) -> Option<&Self::Id> {
		self.redacts.as_ref()
	}
}

/// Prevent derived equality which wouldn't limit itself to event_id
impl Eq for Pdu {}

/// Equality determined by the Pdu's ID, not the memory representations.
impl PartialEq for Pdu {
	fn eq(&self, other: &Self) -> bool {
		self.event_id == other.event_id
	}
}

/// Ordering determined by the Pdu's ID, not the memory representations.
impl Ord for Pdu {
	fn cmp(&self, other: &Self) -> Ordering {
		self.event_id.cmp(&other.event_id)
	}
}

/// Ordering determined by the Pdu's ID, not the memory representations.
impl PartialOrd for Pdu {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}
