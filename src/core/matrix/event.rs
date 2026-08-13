mod content;
mod filter;
mod format;
mod id;
mod redact;
mod relation;
/// State-event key types and ordering helpers.
///
/// Keys combine a state event type with its state key for use in maps. The
/// module also provides forward and reverse comparison functions.
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
	filter::{Matches, trim_event_fields},
	format::{Owned, Ref},
	id::*,
	relation::RelationTypeEqual,
	state_key::{StateKey, TypeStateKey},
	type_ext::TypeExt,
};
use super::pdu::Pdu;
use crate::{Result, utils};

/// Abstraction of a PDU so users can have their own PDU types.
pub trait Event: Clone + Debug + Send + Sync {
	/// Checks whether this event has both the requested type and state key.
	///
	/// Message-like events never match because their state key is absent. The
	/// comparison does not inspect event content.
	#[inline]
	fn is_type_and_state_key(&self, kind: &TimelineEventType, state_key: &str) -> bool {
		self.kind() == kind && self.state_key() == Some(state_key)
	}

	/// Serialize into a Ruma JSON format, consuming.
	#[inline]
	fn into_format<T>(self) -> T
	where
		T: From<Owned<Self>>,
		Self: Sized,
	{
		Owned(self).into()
	}

	/// Serialize into a Ruma JSON format
	#[inline]
	fn to_format<'a, T>(&'a self) -> T
	where
		T: From<Ref<'a, Self>>,
		Self: Sized + 'a,
	{
		Ref(self).into()
	}

	/// Checks an unsigned-data property with a caller-supplied predicate.
	///
	/// The method returns false when the property is absent or the predicate
	/// rejects its value. Missing or malformed unsigned data is treated as
	/// empty.
	#[inline]
	fn contains_unsigned_property<T>(&self, property: &str, is_type: T) -> bool
	where
		T: FnOnce(&JsonValue) -> bool,
		Self: Sized,
	{
		unsigned::contains_unsigned_property::<T, _>(self, property, is_type)
	}

	/// Deserializes one property from the event's unsigned data.
	///
	/// A missing property or malformed unsigned data is reported as not found.
	/// A value of the wrong type fails deserialization.
	#[inline]
	fn get_unsigned_property<T>(&self, property: &str) -> Result<T>
	where
		T: for<'de> Deserialize<'de>,
		Self: Sized,
	{
		unsigned::get_unsigned_property::<T, _>(self, property)
	}

	/// Deserializes the event's unsigned data as a JSON value.
	///
	/// Missing or malformed unsigned data produces JSON null. Use
	/// `get_unsigned` when the distinction must be preserved.
	#[inline]
	fn get_unsigned_as_value(&self) -> JsonValue
	where
		Self: Sized,
	{
		unsigned::get_unsigned_as_value(self)
	}

	/// Deserializes the complete unsigned-data object into a requested type.
	///
	/// Missing unsigned data is reported as not found. Invalid JSON or a type
	/// mismatch fails deserialization.
	#[inline]
	fn get_unsigned<T>(&self) -> Result<T>
	where
		T: for<'de> Deserialize<'de>,
		Self: Sized,
	{
		unsigned::get_unsigned::<T, _>(self)
	}

	/// Deserializes event content as an untyped JSON value.
	///
	/// The returned value owns its data and can be inspected without borrowing
	/// the event. Typed consumers should prefer `get_content`.
	///
	/// # Panics
	///
	/// Panics when the stored content is not valid JSON.
	#[inline]
	fn get_content_as_value(&self) -> JsonValue
	where
		Self: Sized,
	{
		content::as_value(self)
	}

	/// Deserializes event content into a requested type.
	///
	/// The target type determines which event-content shape is accepted.
	/// Invalid JSON or a mismatched target type produces a bad-JSON result.
	#[inline]
	fn get_content<T>(&self) -> Result<T>
	where
		for<'de> T: Deserialize<'de>,
		Self: Sized,
	{
		content::get::<T, _>(self)
	}

	/// Resolves the event ID targeted by a redaction event.
	///
	/// The selected room rules determine whether the ID is read from event
	/// content or the top-level `redacts` field. Non-redaction events and
	/// malformed content return `None`.
	#[inline]
	fn redacts_id(&self, room_rules: &RoomVersionRules) -> Option<OwnedEventId>
	where
		Self: Sized,
	{
		redact::redacts_id(self, room_rules)
	}

	/// Reports whether the event carries redaction metadata.
	///
	/// Redaction is recognized by the presence of `unsigned.redacted_because`.
	/// Missing or malformed unsigned data is treated as not redacted.
	#[inline]
	fn is_redacted(&self) -> bool
	where
		Self: Sized,
	{
		redact::is_redacted(self)
	}

	/// Consumes the event and serializes its PDU as a canonical JSON object.
	///
	/// Implementations first convert the event to the common `Pdu`
	/// representation. The resulting object preserves the stored event fields.
	///
	/// # Panics
	///
	/// Panics if the PDU cannot be serialized as a canonical JSON object.
	#[inline]
	fn into_canonical_object(self) -> CanonicalJsonObject
	where
		Self: Sized,
	{
		utils::to_canonical_object(self.into_pdu()).expect("failed to create Value::Object")
	}

	/// Serializes the event's PDU as an owned canonical JSON object.
	///
	/// The event remains available after conversion. The resulting object
	/// preserves the stored event fields.
	///
	/// # Panics
	///
	/// Panics if the PDU cannot be serialized as a canonical JSON object.
	#[inline]
	fn to_canonical_object(&self) -> CanonicalJsonObject {
		utils::to_canonical_object(self.as_pdu()).expect("failed to create Value::Object")
	}

	/// Consumes the event and serializes its PDU as a JSON value.
	///
	/// Implementations first convert the event to the common `Pdu`
	/// representation. The returned value is an owned JSON object.
	///
	/// # Panics
	///
	/// Panics if the PDU cannot be serialized as JSON.
	#[inline]
	fn into_value(self) -> JsonValue
	where
		Self: Sized,
	{
		serde_json::to_value(self.into_pdu()).expect("failed to create JSON Value")
	}

	/// Serializes the event's PDU as an owned JSON value.
	///
	/// The event remains available after conversion. The returned value is an
	/// owned JSON object.
	///
	/// # Panics
	///
	/// Panics if the PDU cannot be serialized as JSON.
	#[inline]
	fn to_value(&self) -> JsonValue {
		serde_json::to_value(self.as_pdu()).expect("failed to create JSON Value")
	}

	/// Returns mutable access to the common PDU representation.
	///
	/// Implementations backed by a mutable `Pdu` override this method. The
	/// default implementation marks mutable conversion as unsupported.
	///
	/// # Panics
	///
	/// Panics when the implementation does not provide mutable PDU access.
	#[inline]
	fn as_mut_pdu(&mut self) -> &mut Pdu { unimplemented!("not a mutable Pdu") }

	/// Borrows the event as the common PDU representation.
	///
	/// Implementations may return their underlying PDU directly or a borrowed
	/// PDU representation with the same event data.
	fn as_pdu(&self) -> &Pdu;

	/// Converts the event into an owned common PDU representation.
	///
	/// Owned implementations can move their PDU. Borrowed implementations clone
	/// the PDU so the returned value owns every field.
	fn into_pdu(self) -> Pdu;

	/// Reports whether consuming conversion can move an owned PDU.
	///
	/// A false result indicates that `into_pdu` must clone borrowed event data.
	/// The value can help callers choose a conversion path.
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
	/// Returns the event's timeline type.
	///
	/// This compatibility accessor delegates directly to `kind`. New callers
	/// can use either name without changing the returned value.
	#[inline]
	fn event_type(&self) -> &TimelineEventType { self.kind() }
}
