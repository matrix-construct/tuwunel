mod content;
mod filter;
mod format;
mod id;
mod redact;
mod relation;
pub mod state_key;
mod type_ext;
mod unsigned;

use std::fmt::Debug;

use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, UserId,
	events::TimelineEventType, room_version_rules::RoomVersionRules,
};
use serde::Deserialize;
use serde_json::{Value as JsonValue, value::RawValue as RawJsonValue};

pub use self::{
	filter::Matches,
	id::*,
	relation::RelationTypeEqual,
	state_key::{StateKey, TypeStateKey},
	type_ext::TypeExt,
};
use super::pdu::Pdu;
use crate::{Result, utils};

/// Abstraction of a PDU so users can have their own PDU types.
pub trait Event: Clone + Debug + Send + Sync {
	#[inline]
	fn is_type_and_state_key(&self, kind: &TimelineEventType, state_key: &str) -> bool {
		self.kind() == kind && self.state_key() == Some(state_key)
	}

	/// Serialize into a Ruma JSON format, consuming.
	#[inline]
	fn into_format<T>(self) -> T
	where
		T: From<format::Owned<Self>>,
		Self: Sized,
	{
		format::Owned(self).into()
	}

	/// Serialize into a Ruma JSON format
	#[inline]
	fn to_format<'a, T>(&'a self) -> T
	where
		T: From<format::Ref<'a, Self>>,
		Self: Sized + 'a,
	{
		format::Ref(self).into()
	}

	#[inline]
	fn contains_unsigned_property<T>(&self, property: &str, is_type: T) -> bool
	where
		T: FnOnce(&JsonValue) -> bool,
		Self: Sized,
	{
		unsigned::contains_unsigned_property::<T, _>(self, property, is_type)
	}

	#[inline]
	fn get_unsigned_property<T>(&self, property: &str) -> Result<T>
	where
		T: for<'de> Deserialize<'de>,
		Self: Sized,
	{
		unsigned::get_unsigned_property::<T, _>(self, property)
	}

	#[inline]
	fn get_unsigned_as_value(&self) -> JsonValue
	where
		Self: Sized,
	{
		unsigned::get_unsigned_as_value(self)
	}

	#[inline]
	fn get_unsigned<T>(&self) -> Result<T>
	where
		T: for<'de> Deserialize<'de>,
		Self: Sized,
	{
		unsigned::get_unsigned::<T, _>(self)
	}

	#[inline]
	fn get_content_as_value(&self) -> JsonValue
	where
		Self: Sized,
	{
		content::as_value(self)
	}

	#[inline]
	fn get_content<T>(&self) -> Result<T>
	where
		for<'de> T: Deserialize<'de>,
		Self: Sized,
	{
		content::get::<T, _>(self)
	}

	#[inline]
	fn redacts_id(&self, room_rules: &RoomVersionRules) -> Option<OwnedEventId>
	where
		Self: Sized,
	{
		redact::redacts_id(self, room_rules)
	}

	#[inline]
	fn is_redacted(&self) -> bool
	where
		Self: Sized,
	{
		redact::is_redacted(self)
	}

	#[inline]
	fn into_canonical_object(self) -> CanonicalJsonObject
	where
		Self: Sized,
	{
		utils::to_canonical_object(self.into_pdu()).expect("failed to create Value::Object")
	}

	#[inline]
	fn to_canonical_object(&self) -> CanonicalJsonObject {
		utils::to_canonical_object(self.as_pdu()).expect("failed to create Value::Object")
	}

	#[inline]
	fn into_value(self) -> JsonValue
	where
		Self: Sized,
	{
		serde_json::to_value(self.into_pdu()).expect("failed to create JSON Value")
	}

	#[inline]
	fn to_value(&self) -> JsonValue {
		serde_json::to_value(self.as_pdu()).expect("failed to create JSON Value")
	}

	#[inline]
	fn as_mut_pdu(&mut self) -> &mut Pdu { unimplemented!("not a mutable Pdu") }

	fn as_pdu(&self) -> &Pdu;

	fn into_pdu(self) -> Pdu;

	fn is_owned(&self) -> bool;

	//
	// Canonical properties
	//

	/// All the authenticating events for this event.
	fn auth_events(&self) -> impl DoubleEndedIterator<Item = &EventId> + Clone + Send + '_;

	/// All the authenticating events for this event.
	fn auth_events_into(
		self,
	) -> impl IntoIterator<IntoIter = impl Iterator<Item = OwnedEventId>> + Send;

	/// The event's content.
	fn content(&self) -> &RawJsonValue;

	/// The `EventId` of this event.
	fn event_id(&self) -> &EventId;

	/// The time of creation on the originating server.
	fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch;

	/// The events before this event.
	fn prev_events(&self) -> impl DoubleEndedIterator<Item = &EventId> + Clone + Send + '_;

	/// If this event is a redaction event this is the event it redacts.
	fn redacts(&self) -> Option<&EventId>;

	/// see: <https://spec.matrix.org/v1.14/rooms/v11/#rejected-events>
	fn rejected(&self) -> bool;

	/// The `RoomId` of this event.
	fn room_id(&self) -> &RoomId;

	/// The `UserId` of this event.
	fn sender(&self) -> &UserId;

	/// The state key for this event.
	fn state_key(&self) -> Option<&str>;

	/// The event type.
	fn kind(&self) -> &TimelineEventType;

	/// Metadata container; peer-trusted only.
	fn unsigned(&self) -> Option<&RawJsonValue>;

	//#[deprecated]
	#[inline]
	fn event_type(&self) -> &TimelineEventType { self.kind() }
}
