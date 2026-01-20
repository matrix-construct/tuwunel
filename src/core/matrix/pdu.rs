mod builder;
mod count;
pub mod format;
mod hashes;
mod id;
mod raw_id;
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
use smallvec::SmallVec;

pub use self::{
	Count as PduCount, Id as PduId, Pdu as PduEvent, RawId as RawPduId,
	builder::{Builder, Builder as PduBuilder},
	count::Count,
	hashes::EventHashes as EventHash,
	id::Id,
	raw_id::*,
};
use super::{Event, ShortRoomId, StateKey};
use crate::Result;

/// Persistent Data Unit (Event)
#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct Pdu {
	#[serde(rename = "type")]
	pub kind: TimelineEventType,

	pub content: Box<RawJsonValue>,

	pub event_id: OwnedEventId,

	pub room_id: OwnedRoomId,

	pub sender: OwnedUserId,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub state_key: Option<StateKey>,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub redacts: Option<OwnedEventId>,

	pub prev_events: PrevEvents,

	pub auth_events: AuthEvents,

	pub origin_server_ts: UInt,

	pub depth: UInt,

	pub hashes: EventHash,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub origin: Option<OwnedServerName>,

	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub unsigned: Option<Box<RawJsonValue>>,

	// BTreeMap<Box<ServerName>, BTreeMap<ServerSigningKeyId, String>>
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub signatures: Option<Box<RawJsonValue>>,

	//TODO: https://spec.matrix.org/v1.14/rooms/v11/#rejected-events
	#[cfg(test)]
	#[serde(default, skip_serializing)]
	pub rejected: bool,
}

/// Tuned prev_events vector. Most events have one prev_event. Many events have
/// more but allocations for all of those cases still beats allocations for all
/// cases.
pub type PrevEvents = SmallVec<[OwnedEventId; 1]>;

/// Tuned auth_events vector. Average events have three auth events. It is
/// debatable whether this could be an ArrayVec but the realistic upper-bound is
/// too high and non-deterministic in the era of restricted-type rooms.
pub type AuthEvents = SmallVec<[OwnedEventId; 3]>;

/// The [maximum size allowed] for a PDU.
/// [maximum size allowed]: https://spec.matrix.org/latest/client-server-api/#size-limits
pub const MAX_PDU_BYTES: usize = 65_535;

/// The [maximum length allowed] for the `prev_events` array of a PDU.
/// [maximum length allowed]: https://spec.matrix.org/latest/rooms/v1/#event-format
pub const MAX_PREV_EVENTS: usize = 20;

/// The [maximum length allowed] for the `auth_events` array of a PDU.
/// [maximum length allowed]: https://spec.matrix.org/latest/rooms/v1/#event-format
pub const MAX_AUTH_EVENTS: usize = 10;

impl Pdu {
	pub fn from_rid_val(
		room_id: &RoomId,
		event_id: &EventId,
		mut json: CanonicalJsonObject,
	) -> Result<Self> {
		let room_id = CanonicalJsonValue::String(room_id.into());
		json.insert("room_id".into(), room_id);

		Self::from_id_val(event_id, json)
	}

	pub fn from_id_val(event_id: &EventId, mut json: CanonicalJsonObject) -> Result<Self> {
		let event_id = CanonicalJsonValue::String(event_id.into());
		json.insert("event_id".into(), event_id);

		Self::from_val(&json)
	}

	pub fn from_val(json: &CanonicalJsonObject) -> Result<Self> {
		serde_json::to_value(json)
			.and_then(serde_json::from_value)
			.map_err(Into::into)
	}
}

impl Event for Pdu
where
	Self: Send + Sync + 'static,
{
	#[inline]
	fn auth_events(&self) -> impl DoubleEndedIterator<Item = &EventId> + Clone + Send + '_ {
		self.auth_events.iter().map(AsRef::as_ref)
	}

	#[inline]
	fn auth_events_into(
		self,
	) -> impl IntoIterator<IntoIter = impl Iterator<Item = OwnedEventId>> + Send {
		self.auth_events.into_iter()
	}

	#[inline]
	fn content(&self) -> &RawJsonValue { &self.content }

	#[inline]
	fn event_id(&self) -> &EventId { &self.event_id }

	#[inline]
	fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
		MilliSecondsSinceUnixEpoch(self.origin_server_ts)
	}

	#[inline]
	fn prev_events(&self) -> impl DoubleEndedIterator<Item = &EventId> + Clone + Send + '_ {
		self.prev_events.iter().map(AsRef::as_ref)
	}

	#[inline]
	fn redacts(&self) -> Option<&EventId> { self.redacts.as_deref() }

	#[cfg(test)]
	#[inline]
	fn rejected(&self) -> bool { self.rejected }

	#[cfg(not(test))]
	#[inline]
	fn rejected(&self) -> bool { false }

	#[inline]
	fn room_id(&self) -> &RoomId { &self.room_id }

	#[inline]
	fn sender(&self) -> &UserId { &self.sender }

	#[inline]
	fn state_key(&self) -> Option<&str> { self.state_key.as_deref() }

	#[inline]
	fn kind(&self) -> &TimelineEventType { &self.kind }

	#[inline]
	fn unsigned(&self) -> Option<&RawJsonValue> { self.unsigned.as_deref() }

	#[inline]
	fn as_mut_pdu(&mut self) -> &mut Pdu { self }

	#[inline]
	fn as_pdu(&self) -> &Pdu { self }

	#[inline]
	fn into_pdu(self) -> Pdu { self }

	#[inline]
	fn is_owned(&self) -> bool { true }
}

impl Event for &Pdu
where
	Self: Send,
{
	#[inline]
	fn auth_events(&self) -> impl DoubleEndedIterator<Item = &EventId> + Clone + Send + '_ {
		self.auth_events.iter().map(AsRef::as_ref)
	}

	#[inline]
	fn auth_events_into(
		self,
	) -> impl IntoIterator<IntoIter = impl Iterator<Item = OwnedEventId>> + Send {
		self.auth_events.iter().map(ToOwned::to_owned)
	}

	#[inline]
	fn content(&self) -> &RawJsonValue { &self.content }

	#[inline]
	fn event_id(&self) -> &EventId { &self.event_id }

	#[inline]
	fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
		MilliSecondsSinceUnixEpoch(self.origin_server_ts)
	}

	#[inline]
	fn prev_events(&self) -> impl DoubleEndedIterator<Item = &EventId> + Clone + Send + '_ {
		self.prev_events.iter().map(AsRef::as_ref)
	}

	#[inline]
	fn redacts(&self) -> Option<&EventId> { self.redacts.as_deref() }

	#[cfg(test)]
	#[inline]
	fn rejected(&self) -> bool { self.rejected }

	#[cfg(not(test))]
	#[inline]
	fn rejected(&self) -> bool { false }

	#[inline]
	fn room_id(&self) -> &RoomId { &self.room_id }

	#[inline]
	fn sender(&self) -> &UserId { &self.sender }

	#[inline]
	fn state_key(&self) -> Option<&str> { self.state_key.as_deref() }

	#[inline]
	fn kind(&self) -> &TimelineEventType { &self.kind }

	#[inline]
	fn unsigned(&self) -> Option<&RawJsonValue> { self.unsigned.as_deref() }

	#[inline]
	fn as_pdu(&self) -> &Pdu { self }

	#[inline]
	fn into_pdu(self) -> Pdu { self.clone() }

	#[inline]
	fn is_owned(&self) -> bool { false }
}

/// Prevent derived equality which wouldn't limit itself to event_id
impl Eq for Pdu {}

/// Equality determined by the Pdu's ID, not the memory representations.
impl PartialEq for Pdu {
	fn eq(&self, other: &Self) -> bool { self.event_id == other.event_id }
}

/// Ordering determined by the Pdu's ID, not the memory representations.
impl Ord for Pdu {
	fn cmp(&self, other: &Self) -> Ordering { self.event_id.cmp(&other.event_id) }
}

/// Ordering determined by the Pdu's ID, not the memory representations.
impl PartialOrd for Pdu {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
