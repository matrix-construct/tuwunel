mod builder;
mod count;
mod format;
mod hashes;
mod id;
mod raw_id;
#[cfg(test)]
mod tests;
mod unsigned;

use std::cmp::Ordering;

use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId,
	OwnedRoomId, OwnedServerName, OwnedUserId, RoomId, UInt, UserId,
	canonical_json::redact_in_place,
	events::TimelineEventType,
	room_version_rules::{RedactionRules, RoomVersionRules},
	serde::Raw,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue as RawJsonValue;
use smallvec::SmallVec;

pub use self::{
	Count as PduCount, Id as PduId, Pdu as PduEvent, RawId as RawPduId,
	builder::{Builder, Builder as PduBuilder},
	count::Count,
	format::{
		check::{check_room_id, check_rules},
		from_incoming_federation, into_outgoing_federation,
	},
	hashes::EventHashes as EventHash,
	id::Id,
	raw_id::*,
};
use super::{Event, ShortRoomId, StateKey};
use crate::{Result, err};

/// Persistent Data Unit (Event)
#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct Pdu {
	#[serde(rename = "type")]
	pub kind: TimelineEventType,

	pub content: Content,

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
	pub unsigned: Option<Unsigned>,

	//TODO: https://spec.matrix.org/v1.14/rooms/v11/#rejected-events
	#[cfg(test)]
	#[serde(default, skip_serializing)]
	pub rejected: bool,
}

/// Inline storage for the common single-entry `prev_events` case.
///
/// Events with additional predecessors spill to the heap, avoiding larger
/// inline storage on every event.
pub type PrevEvents = SmallVec<[OwnedEventId; 1]>;

/// Inline storage for the typical three-entry `auth_events` case.
///
/// Restricted rooms can require many more entries, so this remains a spilling
/// `SmallVec` rather than a fixed-capacity `ArrayVec`.
pub type AuthEvents = SmallVec<[OwnedEventId; 3]>;

/// Raw event-content storage with 112 bytes of inline capacity.
///
/// The capacity follows an allocator-profile mode in the 96 to 112 byte range
/// and targets a 128 byte total size with `SmallVec` metadata.
pub type Content = Raw<CanonicalJsonObject, 112>;

/// Raw `unsigned` storage with 112 bytes of inline capacity.
///
/// The enclosing field is usually `None` or contains a small local annotation,
/// such as `transaction_id`, `age`, or `membership`. Those values remain inline
/// at the `Content` size class, while larger state-event `prev_content` and
/// bundled `m.relations` values spill to the heap.
pub type Unsigned = Raw<CanonicalJsonObject, 112>;

/// The [maximum size allowed] for a PDU.
/// [maximum size allowed]: <https://spec.matrix.org/latest/client-server-api/#size-limits>
pub const MAX_PDU_BYTES: usize = 65_535;

/// The [maximum length allowed] for the `prev_events` array of a PDU.
/// [maximum length allowed]: <https://spec.matrix.org/latest/rooms/v1/#event-format>
pub const MAX_PREV_EVENTS: usize = 20;

/// The [maximum length allowed] for the `auth_events` array of a PDU.
/// [maximum length allowed]: <https://spec.matrix.org/latest/rooms/v1/#event-format>
pub const MAX_AUTH_EVENTS: usize = 10;

impl Pdu {
	pub fn from_object_and_roomid_and_eventid(
		room_id: &RoomId,
		event_id: &EventId,
		mut json: CanonicalJsonObject,
	) -> Result<Self> {
		let room_id = CanonicalJsonValue::String(room_id.into());
		json.insert("room_id".into(), room_id);
		Self::from_object_and_eventid(event_id, json)
	}

	pub fn from_object_and_eventid(
		event_id: &EventId,
		mut json: CanonicalJsonObject,
	) -> Result<Self> {
		let event_id = CanonicalJsonValue::String(event_id.into());
		json.insert("event_id".into(), event_id);
		Self::from_object(json)
	}

	pub fn from_object_federation(
		room_id: &RoomId,
		event_id: &EventId,
		json: CanonicalJsonObject,
		rules: &RoomVersionRules,
	) -> Result<(Self, CanonicalJsonObject)> {
		let json = from_incoming_federation(room_id, event_id, json, rules);
		let pdu = Self::from_object_checked(json.clone(), rules)?;
		check_room_id(&pdu, room_id)?;
		Ok((pdu, json))
	}

	pub fn from_object_checked(
		json: CanonicalJsonObject,
		rules: &RoomVersionRules,
	) -> Result<Self> {
		check_rules(&json, &rules.event_format)?;
		Self::from_object(json)
	}

	pub fn from_object(json: CanonicalJsonObject) -> Result<Self> {
		let json = CanonicalJsonValue::Object(json);
		Self::from_value(json)
	}

	pub fn from_raw_value(json: &RawJsonValue) -> Result<Self> {
		let json: CanonicalJsonValue = json.into();
		Self::from_value(json)
	}

	pub fn from_value(json: CanonicalJsonValue) -> Result<Self> {
		serde_json::from_value(json.into()).map_err(Into::into)
	}

	pub fn from_raw_json(json: &RawJsonValue) -> Result<Self> {
		Self::deserialize(json).map_err(Into::into)
	}

	/// MSC4025: a pruned clone per the redaction rules, carrying no
	/// `redacted_because`; no redaction event exists for a serve-time
	/// erasure.
	pub fn redacted(&self, rules: &RedactionRules) -> Result<Self> {
		let mut object = self.to_canonical_object();

		redact_in_place(&mut object, rules, None)
			.map_err(|e| err!("Failed to redact event: {e}"))?;

		Self::from_object(object)
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
	fn content(&self) -> &RawJsonValue { self.content.json() }

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
	fn unsigned(&self) -> Option<&RawJsonValue> { self.unsigned.as_ref().map(Unsigned::json) }

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
	fn content(&self) -> &RawJsonValue { self.content.json() }

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
	fn unsigned(&self) -> Option<&RawJsonValue> { self.unsigned.as_ref().map(Unsigned::json) }

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
