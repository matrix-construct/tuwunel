use std::{
	borrow::Cow,
	collections::{BTreeMap, btree_map::Entry},
	fmt::{Formatter, Result as FmtResult},
	iter::from_fn,
};

use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, UserId,
	events::{AnySyncMessageLikeEvent, room::member::MembershipState},
	serde::Raw,
};
use serde::{
	Deserialize, Deserializer, Serialize, Serializer,
	de::{DeserializeSeed, Error as DeError, IgnoredAny, MapAccess, Visitor},
	ser::SerializeMap,
};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue, to_raw_value};
use tuwunel_core::{Result, err, implement, utils::BoolExt};

use super::{Pdu, Unsigned};

type BorrowedObject<'a> = BTreeMap<Cow<'a, str>, &'a RawJsonValue>;
type JsonEntry<'a> = (JsonString<'a>, &'a RawJsonValue);

struct RelationBundle<'a> {
	unsigned: BorrowedObject<'a>,
	relations: BorrowedObject<'a>,
}

struct ThreadBundleFields<'a> {
	unsigned: BorrowedObject<'a>,
	relations: BorrowedObject<'a>,
	thread: BorrowedObject<'a>,
	latest_event: BorrowedObject<'a>,
	latest_event_unsigned: BorrowedObject<'a>,
}

struct RawObjectPatch<'a, 'raw, T> {
	object: &'a BorrowedObject<'raw>,
	field: &'static str,
	value: T,
}

struct RawObjectRemove<'a, 'raw> {
	object: &'a BorrowedObject<'raw>,
	field: &'a str,
}

struct BorrowedField<'a>(&'a str);

struct UniqueObject;

struct JsonString<'a>(Cow<'a, str>);

struct JsonStringVisitor;

#[derive(Serialize)]
struct ReferenceBundle<'a> {
	chunk: ReferenceChunk<'a>,
}

struct ReferenceChunk<'a>(&'a [OwnedEventId]);

#[derive(Serialize)]
struct ReferenceEvent<'a> {
	event_id: &'a OwnedEventId,
}

#[derive(Deserialize)]
struct Identity {
	event_id: OwnedEventId,
	sender: OwnedUserId,
}

/// Removes the transaction ID unless the event is served to its own sender.
///
/// The token is the sending client's own idempotency value, which it matches
/// its echo against, so the sender keeps it and nobody else sees it. A `None`
/// requester is nobody in particular and is treated as somebody else.
#[implement(Pdu)]
pub fn remove_transaction_id_unless_sender(&mut self, user_id: Option<&UserId>) -> Result {
	user_id
		.is_none_or(|user_id| self.sender != *user_id)
		.then(|| self.remove_transaction_id())
		.unwrap_or(Ok(()))
}

/// Removes the local transaction ID from unsigned event metadata.
///
/// Other unsigned properties are retained and the object is re-encoded. An
/// event without unsigned data, or without the key, is left unchanged.
#[implement(Pdu)]
pub fn remove_transaction_id(&mut self) -> Result {
	use BTreeMap as Map;

	let Some(unsigned) = &self.unsigned else {
		return Ok(());
	};

	let raw = unsigned.json().get();
	if !raw.contains("\"transaction_id\"") {
		return Ok(());
	}

	let mut unsigned: Map<&str, Raw<JsonValue>> = serde_json::from_str(raw)
		.map_err(|e| err!(Database("Invalid unsigned in pdu event: {e}")))?;

	unsigned.remove("transaction_id");
	self.unsigned = to_raw_value(&unsigned)
		.map(Into::into)
		.map(Some)
		.expect("unsigned is valid");

	Ok(())
}

/// State-section serving strips the stored `prev_content`/`prev_sender`
/// pair, dropping `unsigned` entirely when emptied; timeline serving keeps
/// the trio.
#[implement(Pdu)]
pub fn remove_prev_state(&mut self) -> Result {
	use BTreeMap as Map;

	let Some(unsigned) = &self.unsigned else {
		return Ok(());
	};

	let raw = unsigned.json().get();
	let prev_keys = raw.contains("\"prev_content\"") || raw.contains("\"prev_sender\"");
	if !prev_keys && raw != "{}" {
		return Ok(());
	}

	let mut unsigned: Map<&str, Raw<JsonValue>> = serde_json::from_str(raw)
		.map_err(|e| err!(Database("Invalid unsigned in pdu event: {e}")))?;

	unsigned.remove("prev_content");
	unsigned.remove("prev_sender");
	self.unsigned = unsigned
		.is_empty()
		.is_false()
		.then(|| to_raw_value(&unsigned))
		.transpose()?
		.map(Into::into);

	Ok(())
}

/// Adds the event's current age to unsigned metadata.
///
/// Age is the saturating millisecond difference between the current time and
/// `origin_server_ts`. Future timestamps can therefore produce a negative
/// value.
#[implement(Pdu)]
pub fn add_age(&mut self) -> Result {
	use BTreeMap as Map;

	let mut unsigned: Map<&str, Raw<JsonValue>> = self
		.unsigned
		.as_ref()
		.map(Unsigned::json)
		.map(RawJsonValue::get)
		.map_or_else(|| Ok(Map::new()), serde_json::from_str)
		.map_err(|e| err!(Database("Invalid unsigned in pdu event: {e}")))?;

	// deliberately allowing for the possibility of negative age
	let now: i128 = MilliSecondsSinceUnixEpoch::now().get().into();
	let then: i128 = self.origin_server_ts.into();
	let this_age = now.saturating_sub(then);

	unsigned.insert("age", raw_of(&this_age)?);
	self.unsigned = Some(to_raw_value(&unsigned)?.into());

	Ok(())
}

/// MSC4115: annotate the served event with the requesting user's room
/// membership at the time of the event.
#[implement(Pdu)]
pub fn add_membership(&mut self, membership: &MembershipState) -> Result {
	use BTreeMap as Map;

	let mut unsigned: Map<&str, Raw<JsonValue>> = self
		.unsigned
		.as_ref()
		.map(Unsigned::json)
		.map(RawJsonValue::get)
		.map_or_else(|| Ok(Map::new()), serde_json::from_str)
		.map_err(|e| err!(Database("Invalid unsigned in pdu event: {e}")))?;

	unsigned.insert("membership", raw_of(membership)?);
	self.unsigned = Some(to_raw_value(&unsigned)?.into());

	Ok(())
}

/// Adds or replaces a named bundled relation in unsigned metadata.
///
/// The related PDU is serialized under `unsigned.m.relations`; `None` stores an
/// empty object for the named relation. Existing unsigned properties are
/// retained.
#[implement(Pdu)]
pub fn add_relation(&mut self, name: &str, pdu: Option<&Pdu>) -> Result {
	use serde_json::Map;

	let mut unsigned: Map<String, JsonValue> = self
		.unsigned
		.as_ref()
		.map(Unsigned::json)
		.map(RawJsonValue::get)
		.map_or_else(|| Ok(Map::new()), serde_json::from_str)
		.map_err(|e| err!(Database("Invalid unsigned in pdu event: {e}")))?;

	let pdu = pdu
		.map(serde_json::to_value)
		.transpose()?
		.unwrap_or_else(|| JsonValue::Object(Map::new()));

	unsigned
		.entry("m.relations")
		.or_insert(JsonValue::Object(Map::new()))
		.as_object_mut()
		.map(|object| object.insert(name.to_owned(), pdu));

	self.unsigned = Some(to_raw_value(&unsigned)?.into());

	Ok(())
}

/// MSC3816: overwrite `unsigned.m.relations.m.thread.current_user_participated`
/// with a per-requester value. No-op when the event carries no thread bundle.
#[implement(Pdu)]
pub fn set_thread_participated(&mut self, participated: bool) -> Result {
	use serde_json::Map;

	let Some(unsigned) = self.unsigned.as_ref() else {
		return Ok(());
	};

	let mut unsigned: Map<String, JsonValue> = serde_json::from_str(unsigned.json().get())
		.map_err(|e| err!(Database("Invalid unsigned in pdu event: {e}")))?;

	let updated = unsigned
		.get_mut("m.relations")
		.and_then(JsonValue::as_object_mut)
		.and_then(|relations| relations.get_mut("m.thread"))
		.and_then(JsonValue::as_object_mut)
		.map(|thread| {
			thread.insert("current_user_participated".to_owned(), participated.into());
		})
		.is_some();

	if updated {
		self.unsigned = Some(to_raw_value(&unsigned)?.into());
	}

	Ok(())
}

/// Identifies the sender and event ID of a bundled thread preview.
///
/// MSC4025 uses the sender for the erasure check and the event ID to load the
/// event. The thread writer derives both fields from one validated event.
#[implement(Pdu)]
#[inline]
pub fn thread_latest_event(&self) -> Result<Option<(OwnedEventId, OwnedUserId)>> {
	self.unsigned
		.as_ref()
		.map(Unsigned::json)
		.map(thread_latest)
		.transpose()?
		.flatten()
		.map(thread_identity)
		.transpose()
}

/// Whether `unsigned.m.relations` contains an `m.thread` bundle.
///
/// This decodes object keys, so escaped spellings cannot bypass read-time
/// privacy handling.
#[implement(Pdu)]
pub fn has_thread_bundle(&self) -> Result<bool> {
	self.unsigned
		.as_ref()
		.map(Unsigned::json)
		.map(thread_bundle)
		.transpose()
		.map(|thread| thread.flatten().is_some())
}

/// MSC4025: overwrite `unsigned.m.relations.m.thread.latest_event`, serving
/// the pruned form of an erased sender's thread activity. No-op when the
/// event carries no thread bundle.
#[implement(Pdu)]
pub fn set_thread_latest_event(&mut self, latest: &Raw<AnySyncMessageLikeEvent>) -> Result {
	use serde_json::Map;

	let Some(unsigned) = self.unsigned.as_ref() else {
		return Ok(());
	};

	let latest = serde_json::to_value(latest)?;

	let mut unsigned: Map<String, JsonValue> = serde_json::from_str(unsigned.json().get())
		.map_err(|e| err!(Database("Invalid unsigned in pdu event: {e}")))?;

	let updated = unsigned
		.get_mut("m.relations")
		.and_then(JsonValue::as_object_mut)
		.and_then(|relations| relations.get_mut("m.thread"))
		.and_then(JsonValue::as_object_mut)
		.map(|thread| {
			thread.insert("latest_event".to_owned(), latest);
		})
		.is_some();

	if updated {
		self.unsigned = Some(to_raw_value(&unsigned)?.into());
	}

	Ok(())
}

/// Removes a thread preview transaction ID unless served to its own sender.
///
/// Stored thread bundles predate write-side sanitization, so this rewrites the
/// nested event at serve time while retaining its other unsigned properties.
/// A missing or invalid sender returns an error so callers can fail closed.
#[implement(Pdu)]
pub fn remove_thread_latest_transaction_id_unless_sender(&mut self, user_id: &UserId) -> Result {
	if let Some(unsigned) = self
		.unsigned
		.as_ref()
		.map(Unsigned::json)
		.map(|raw| thread_without_transaction_id(raw, user_id))
		.transpose()?
		.flatten()
	{
		self.unsigned = Some(unsigned);
	}

	Ok(())
}

fn thread_without_transaction_id(
	raw: &RawJsonValue,
	user_id: &UserId,
) -> Result<Option<Unsigned>> {
	thread_transaction_sender(raw)?
		.map(|sender| serde_json::from_str(sender.get()))
		.transpose()
		.map_err(|error| err!(Database("Invalid sender in thread latest event: {error}")))?
		.filter(|JsonString(sender)| sender != user_id.as_str())
		.map(|JsonString(sender)| {
			UserId::parse(sender.as_ref())
				.map_err(|error| err!(Database("Invalid sender in thread latest event: {error}")))
				.and_then(|_| ThreadBundleFields::parse(raw)?.without_transaction_id())
		})
		.transpose()
}

fn thread_transaction_sender(raw: &RawJsonValue) -> Result<Option<&RawJsonValue>> {
	thread_latest(raw)?
		.map(transaction_sender)
		.transpose()
		.map(Option::flatten)
}

fn transaction_sender(latest_event: &RawJsonValue) -> Result<Option<&RawJsonValue>> {
	let sender = || {
		raw_field(latest_event, "sender", "thread latest event")?.ok_or_else(|| {
			err!(Database("Thread latest event with transaction ID has no sender"))
		})
	};

	raw_field(latest_event, "unsigned", "thread latest event")?
		.map(|unsigned| raw_field(unsigned, "transaction_id", "thread latest event unsigned"))
		.transpose()?
		.flatten()
		.map(|_| sender())
		.transpose()
}

fn thread_latest(raw: &RawJsonValue) -> Result<Option<&RawJsonValue>> {
	thread_bundle(raw)?
		.map(|thread| raw_field(thread, "latest_event", "thread bundle"))
		.transpose()
		.map(Option::flatten)
}

fn thread_bundle(raw: &RawJsonValue) -> Result<Option<&RawJsonValue>> {
	raw_field(raw, "m.relations", "unsigned")?
		.map(|relations| raw_field(relations, "m.thread", "bundled relations"))
		.transpose()
		.map(Option::flatten)
}

fn thread_identity(raw: &RawJsonValue) -> Result<(OwnedEventId, OwnedUserId)> {
	serde_json::from_str(raw.get())
		.map(|Identity { event_id, sender }| (event_id, sender))
		.map_err(|error| err!(Database("Invalid thread latest event in PDU event: {error}")))
}

/// Excise `m.thread` from `unsigned.m.relations`, retaining unrelated data.
///
/// Empty relation and unsigned objects are removed with the bundle.
#[implement(Pdu)]
pub fn remove_thread_bundle(&mut self) -> Result {
	remove_relation_bundle(self, "m.thread")
		.or_else(|_| remove_relation_bundle_canonical(self, "m.thread"))
}

/// Canonicalize malformed or ambiguous relation objects while removing only
/// the requested relation. This allocation is reserved for the error path of
/// the borrowed surgical rewrite.
fn remove_relation_bundle_canonical(pdu: &mut Pdu, relation_type: &str) -> Result {
	let Some(raw) = pdu.unsigned.as_ref() else {
		return Ok(());
	};

	let mut unsigned: JsonValue = serde_json::from_str(raw.json().get())?;
	let unsigned = unsigned
		.as_object_mut()
		.ok_or_else(|| err!(Database("Invalid unsigned object in PDU event")))?;

	let remove_relations = match unsigned.get_mut("m.relations") {
		| None => return Ok(()),
		| Some(JsonValue::Object(relations)) => {
			relations.remove(relation_type);
			relations.is_empty()
		},
		| Some(_) => true,
	};

	if remove_relations {
		unsigned.remove("m.relations");
	}

	pdu.unsigned = if unsigned.is_empty() {
		None
	} else {
		Some(raw_as(unsigned)?)
	};

	Ok(())
}

fn remove_relation_bundle(pdu: &mut Pdu, relation_type: &str) -> Result {
	if let Some(unsigned) = pdu
		.unsigned
		.as_ref()
		.map(Unsigned::json)
		.map(|raw| RelationBundle::parse(raw, relation_type))
		.transpose()?
		.flatten()
		.map(|bundle| bundle.without(relation_type))
		.transpose()?
	{
		pdu.unsigned = unsigned;
	}

	Ok(())
}

#[implement(RelationBundle, generics = "<'a>", params = "<'a>")]
fn parse(raw: &'a RawJsonValue, relation_type: &str) -> Result<Option<Self>> {
	let unsigned = raw_object(raw, "unsigned")?;

	unsigned
		.get("m.relations")
		.copied()
		.map(|relations| raw_object(relations, "bundled relations"))
		.transpose()
		.map(|relations| {
			relations
				.filter(|relations| relations.contains_key(relation_type))
				.map(|relations| Self { unsigned, relations })
		})
}

#[implement(RelationBundle, generics = "<'a>", params = "<'a>")]
fn without(&self, relation_type: &str) -> Result<Option<Unsigned>> {
	match self.relations.len() {
		| 1 => self
			.unsigned
			.len()
			.ne(&1)
			.then(|| raw_as(&RawObjectRemove::new(&self.unsigned, "m.relations")))
			.transpose(),
		| _ => {
			let relations = RawObjectRemove::new(&self.relations, relation_type);
			let unsigned = RawObjectPatch::new(&self.unsigned, "m.relations", relations);

			raw_as(&unsigned).map(Some)
		},
	}
}

/// MSC3856: overwrite `unsigned.m.relations.m.thread.count` with a
/// per-requester value excluding ignored senders' replies. No-op when the
/// event carries no thread bundle.
#[implement(Pdu)]
pub fn set_thread_count(&mut self, count: usize) -> Result {
	use serde_json::Map;

	let Some(unsigned) = self.unsigned.as_ref() else {
		return Ok(());
	};

	let mut unsigned: Map<String, JsonValue> = serde_json::from_str(unsigned.json().get())
		.map_err(|e| err!(Database("Invalid unsigned in pdu event: {e}")))?;

	let updated = unsigned
		.get_mut("m.relations")
		.and_then(JsonValue::as_object_mut)
		.and_then(|relations| relations.get_mut("m.thread"))
		.and_then(JsonValue::as_object_mut)
		.map(|thread| {
			thread.insert("count".to_owned(), count.into());
		})
		.is_some();

	if updated {
		self.unsigned = Some(to_raw_value(&unsigned)?.into());
	}

	Ok(())
}

/// MSC3925: fold the newest `m.replace` edit into
/// `unsigned.m.relations.m.replace` as the full replacement event, preserving
/// an existing bundle such as `m.thread` and creating `unsigned` when absent.
#[implement(Pdu)]
pub fn set_replacement_bundle(&mut self, replacement: &Raw<AnySyncMessageLikeEvent>) -> Result {
	use BTreeMap as Map;

	type Object = Map<String, Raw<JsonValue>>;

	let parse = |raw: &RawJsonValue| -> Result<Object> {
		serde_json::from_str(raw.get())
			.map_err(|e| err!(Database("Invalid object in pdu unsigned: {e}")))
	};

	let mut unsigned: Object = self
		.unsigned
		.as_ref()
		.map(|unsigned| parse(unsigned.json()))
		.transpose()?
		.unwrap_or_default();

	let mut relations: Object = unsigned
		.get("m.relations")
		.map(|relations| parse(relations.json()))
		.transpose()?
		.unwrap_or_default();

	relations.insert("m.replace".to_owned(), replacement.cast_ref().clone());
	unsigned.insert("m.relations".to_owned(), to_raw_value(&relations)?.into());
	self.unsigned = Some(to_raw_value(&unsigned)?.into());

	Ok(())
}

/// Inverse of `set_replacement_bundle`: excise `m.replace` from
/// `unsigned.m.relations`, dropping `m.relations` when the excision empties
/// it and `unsigned` when that leaves nothing.
#[implement(Pdu)]
pub fn remove_replacement_bundle(&mut self) -> Result {
	remove_relation_bundle(self, "m.replace")
}

/// MSC2675/MSC3267: fold reference relations into
/// `unsigned.m.relations.m.reference` as `{ chunk: [{ event_id }, ...] }`,
/// preserving an existing bundle such as `m.thread` or `m.replace` and creating
/// `unsigned` when absent.
#[implement(Pdu)]
pub fn set_reference_bundle(&mut self, event_ids: &[OwnedEventId]) -> Result {
	let unsigned = self
		.unsigned
		.as_ref()
		.map(|unsigned| raw_object(unsigned.json(), "unsigned"))
		.transpose()?
		.unwrap_or_default();

	let relations = unsigned
		.get("m.relations")
		.copied()
		.map(|relations| raw_object(relations, "bundled relations"))
		.transpose()?
		.unwrap_or_default();

	let reference = ReferenceBundle { chunk: ReferenceChunk(event_ids) };
	let relations = RawObjectPatch::new(&relations, "m.reference", reference);
	let unsigned = RawObjectPatch::new(&unsigned, "m.relations", relations);
	self.unsigned = Some(raw_as(&unsigned)?);

	Ok(())
}

impl<'de> Deserialize<'de> for JsonString<'de> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		deserializer.deserialize_str(JsonStringVisitor)
	}
}

impl<'de> Visitor<'de> for JsonStringVisitor {
	type Value = JsonString<'de>;

	fn expecting(&self, formatter: &mut Formatter<'_>) -> FmtResult {
		formatter.write_str("a JSON string")
	}

	fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E> {
		Ok(JsonString(Cow::Borrowed(value)))
	}

	fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
	where
		E: DeError,
	{
		Ok(JsonString(Cow::Owned(value.to_owned())))
	}

	fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
		Ok(JsonString(Cow::Owned(value)))
	}
}

impl<'de> DeserializeSeed<'de> for BorrowedField<'_> {
	type Value = Option<&'de RawJsonValue>;

	fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
	where
		D: Deserializer<'de>,
	{
		deserializer.deserialize_map(self)
	}
}

impl<'de> Visitor<'de> for BorrowedField<'_> {
	type Value = Option<&'de RawJsonValue>;

	fn expecting(&self, formatter: &mut Formatter<'_>) -> FmtResult {
		write!(formatter, "an object containing {}", self.0)
	}

	fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
	where
		A: MapAccess<'de>,
	{
		let mut value = None; // MapAccess key/value reads share the cursor.

		while let Some(JsonString(field)) = map.next_key()? {
			value = match field.as_ref() {
				| field if field != self.0 => map.next_value().map(|_: IgnoredAny| value),
				| _ if value.is_some() =>
					Err(A::Error::custom(format_args!("duplicate field `{field}`"))),
				| _ => map.next_value().map(Some),
			}?;
		}

		Ok(value)
	}
}

impl<'de> DeserializeSeed<'de> for UniqueObject {
	type Value = BorrowedObject<'de>;

	fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
	where
		D: Deserializer<'de>,
	{
		deserializer.deserialize_map(self)
	}
}

impl<'de> Visitor<'de> for UniqueObject {
	type Value = BorrowedObject<'de>;

	fn expecting(&self, formatter: &mut Formatter<'_>) -> FmtResult {
		formatter.write_str("an object without duplicate fields")
	}

	fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
	where
		A: MapAccess<'de>,
	{
		from_fn(|| map.next_entry().transpose()).try_fold(
			BorrowedObject::new(),
			|mut object, entry: Result<JsonEntry<'de>, A::Error>| {
				let (JsonString(field), value) = entry?;

				match object.entry(field) {
					| Entry::Occupied(entry) =>
						Err(A::Error::custom(format_args!("duplicate field `{}`", entry.key()))),
					| Entry::Vacant(entry) => {
						entry.insert(value);

						Ok(object)
					},
				}
			},
		)
	}
}

#[implement(ThreadBundleFields, generics = "<'a>", params = "<'a>")]
fn parse(raw: &'a RawJsonValue) -> Result<Self> {
	let child = |object: &BorrowedObject<'a>, field, name| {
		object
			.get(field)
			.ok_or_else(|| {
				err!(Database("Thread transaction probe disagreed with bundle parser"))
			})
			.and_then(|raw| raw_object(raw, name))
	};

	let unsigned = raw_object(raw, "unsigned")?;
	let relations = child(&unsigned, "m.relations", "bundled relations")?;
	let thread = child(&relations, "m.thread", "thread bundle")?;
	let latest_event = child(&thread, "latest_event", "thread latest event")?;
	let latest_event_unsigned = child(&latest_event, "unsigned", "thread latest event unsigned")?;

	Ok(Self {
		unsigned,
		relations,
		thread,
		latest_event,
		latest_event_unsigned,
	})
}

#[implement(ThreadBundleFields, generics = "<'a>", params = "<'a>")]
fn without_transaction_id<U, const N: usize>(&self) -> Result<Raw<U, N>> {
	let latest_event_unsigned =
		RawObjectRemove::new(&self.latest_event_unsigned, "transaction_id");

	let latest_event = RawObjectPatch::new(&self.latest_event, "unsigned", latest_event_unsigned);
	let thread = RawObjectPatch::new(&self.thread, "latest_event", latest_event);
	let relations = RawObjectPatch::new(&self.relations, "m.thread", thread);
	let unsigned = RawObjectPatch::new(&self.unsigned, "m.relations", relations);

	raw_as(&unsigned)
}

impl<'a, 'raw, T> RawObjectPatch<'a, 'raw, T> {
	fn new(object: &'a BorrowedObject<'raw>, field: &'static str, value: T) -> Self {
		Self { object, field, value }
	}
}

impl<T: Serialize> Serialize for RawObjectPatch<'_, '_, T> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let present = self.object.contains_key(self.field);
		let len = self
			.object
			.len()
			.saturating_add(usize::from(!present));

		let map = self.object.iter().try_fold(
			serializer.serialize_map(Some(len))?,
			|mut map, (field, value)| {
				match field.as_ref() == self.field {
					| true => map.serialize_entry(field, &self.value),
					| false => map.serialize_entry(field, value),
				}?;

				Ok(map)
			},
		)?;

		present
			.is_false()
			.then_some(self.field)
			.into_iter()
			.try_fold(map, |mut map, field| {
				map.serialize_entry(field, &self.value)?;

				Ok(map)
			})
			.and_then(SerializeMap::end)
	}
}

impl<'a, 'raw> RawObjectRemove<'a, 'raw> {
	fn new(object: &'a BorrowedObject<'raw>, field: &'a str) -> Self { Self { object, field } }
}

impl Serialize for RawObjectRemove<'_, '_> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		self.object
			.iter()
			.filter(|(field, _)| field.as_ref() != self.field)
			.try_fold(
				serializer.serialize_map(Some(self.object.len().saturating_sub(1)))?,
				|mut map, (field, value)| {
					map.serialize_entry(field, value)?;

					Ok(map)
				},
			)
			.and_then(SerializeMap::end)
	}
}

impl Serialize for ReferenceChunk<'_> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.collect_seq(
			self.0
				.iter()
				.map(|event_id| ReferenceEvent { event_id }),
		)
	}
}

fn raw_object<'a>(raw: &'a RawJsonValue, name: &str) -> Result<BorrowedObject<'a>> {
	deserialize_raw(raw, UniqueObject, name)
}

fn raw_field<'a>(
	raw: &'a RawJsonValue,
	field: &str,
	name: &str,
) -> Result<Option<&'a RawJsonValue>> {
	deserialize_raw(raw, BorrowedField(field), name)
}

fn deserialize_raw<'a, D>(raw: &'a RawJsonValue, seed: D, name: &str) -> Result<D::Value>
where
	D: DeserializeSeed<'a>,
{
	let mut deserializer = serde_json::Deserializer::from_str(raw.get());

	seed.deserialize(&mut deserializer)
		.and_then(|value| deserializer.end().map(|()| value))
		.map_err(|error| err!(Database("Invalid {name} object in PDU event: {error}")))
}

/// Serializes `value` into raw JSON labeled as `U`.
///
/// `Raw<T, N>` has identical layout for every phantom `T` at fixed `N`.
/// Callers must ensure that the serialized JSON is valid for `U`.
#[inline]
fn raw_as<T, U, const N: usize>(value: &T) -> Result<Raw<U, N>>
where
	T: Serialize,
{
	Raw::<T, N>::new(value)
		.map(|raw| raw.cast_ref_unchecked::<U>().clone())
		.map_err(Into::into)
}

#[inline]
fn raw_of<T: Serialize>(value: &T) -> Result<Raw<JsonValue>> { raw_as(value) }
