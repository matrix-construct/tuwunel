use std::collections::BTreeMap;

use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
	events::{AnySyncMessageLikeEvent, room::member::MembershipState},
	serde::Raw,
};
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue, to_raw_value};

use super::{Pdu, Unsigned};
use crate::{Result, err, implement, utils::BoolExt};

#[implement(Pdu)]
pub fn remove_transaction_id(&mut self) -> Result {
	use BTreeMap as Map;

	let Some(unsigned) = &self.unsigned else {
		return Ok(());
	};

	let mut unsigned: Map<&str, Raw<JsonValue>> = serde_json::from_str(unsigned.json().get())
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

/// MSC4025: identify the bundled `m.thread` `latest_event` without parsing
/// the whole bundle: the `sender` keys the erasure gate, the `event_id` loads
/// the event on a hit.
#[implement(Pdu)]
#[must_use]
pub fn thread_latest_event(&self) -> Option<(OwnedEventId, OwnedUserId)> {
	#[derive(Deserialize)]
	struct Relations {
		#[serde(rename = "m.thread")]
		thread: Option<Thread>,
	}

	#[derive(Deserialize)]
	struct Thread {
		latest_event: Option<Identity>,
	}

	#[derive(Deserialize)]
	struct Identity {
		event_id: OwnedEventId,
		sender: OwnedUserId,
	}

	let relations: Relations = self
		.unsigned
		.as_ref()?
		.get_field("m.relations")
		.ok()
		.flatten()?;

	let identity = relations.thread?.latest_event?;

	Some((identity.event_id, identity.sender))
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

/// MSC2675/MSC3267: fold reference relations into
/// `unsigned.m.relations.m.reference` as `{ chunk: [{ event_id }, ...] }`,
/// preserving an existing bundle such as `m.thread` or `m.replace` and creating
/// `unsigned` when absent.
#[implement(Pdu)]
pub fn set_reference_bundle(&mut self, event_ids: &[OwnedEventId]) -> Result {
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

	let chunk: Vec<JsonValue> = event_ids
		.iter()
		.map(|event_id| serde_json::json!({ "event_id": event_id }))
		.collect();

	let reference = serde_json::json!({ "chunk": chunk });

	relations.insert("m.reference".to_owned(), to_raw_value(&reference)?.into());
	unsigned.insert("m.relations".to_owned(), to_raw_value(&relations)?.into());
	self.unsigned = Some(to_raw_value(&unsigned)?.into());

	Ok(())
}

#[inline]
fn raw_of<T: Serialize>(value: &T) -> Result<Raw<JsonValue>> {
	Ok(Raw::from_raw_value(&to_raw_value(value)?))
}
