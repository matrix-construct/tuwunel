use std::collections::BTreeMap;

use ruma::MilliSecondsSinceUnixEpoch;
use serde::Deserialize;
use serde_json::value::{to_raw_value, RawValue as RawJsonValue, Value as JsonValue};

use super::Pdu;
use crate::{err, implement, is_true, Result};

#[implement(Pdu)]
pub fn remove_transaction_id(&mut self) -> Result {
	let Some(unsigned) = &self.unsigned else {
		return Ok(());
	};

	let mut unsigned: BTreeMap<String, Box<RawJsonValue>> = serde_json::from_str(unsigned.get())
		.map_err(|e| err!(Database("Invalid unsigned in pdu event: {e}")))?;

	unsigned.remove("transaction_id");
	self.unsigned = to_raw_value(&unsigned)
		.map(Some)
		.expect("unsigned is valid");

	Ok(())
}

#[implement(Pdu)]
pub fn add_age(&mut self) -> Result {
	let mut unsigned: BTreeMap<String, Box<RawJsonValue>> = self
		.unsigned
		.as_ref()
		.map_or_else(|| Ok(BTreeMap::new()), |u| serde_json::from_str(u.get()))
		.map_err(|e| err!(Database("Invalid unsigned in pdu event: {e}")))?;

	// deliberately allowing for the possibility of negative age
	let now: i128 = MilliSecondsSinceUnixEpoch::now().get().into();
	let then: i128 = self.origin_server_ts.into();
	let this_age = now.saturating_sub(then);

	unsigned.insert("age".to_owned(), to_raw_value(&this_age).expect("age is valid"));
	self.unsigned = to_raw_value(&unsigned)
		.map(Some)
		.expect("unsigned is valid");

	Ok(())
}

#[implement(Pdu)]
pub fn add_relation(&mut self, name: &str, pdu: Option<&Pdu>) -> Result {
	use serde_json::Map;

	let mut unsigned: Map<String, JsonValue> = self
		.unsigned
		.as_ref()
		.map_or_else(|| Ok(Map::new()), |u| serde_json::from_str(u.get()))
		.map_err(|e| err!(Database("Invalid unsigned in pdu event: {e}")))?;

	let pdu = pdu
		.map(serde_json::to_value)
		.transpose()?
		.unwrap_or_else(|| JsonValue::Object(Map::new()));

	unsigned
		.entry("m.relations")
		.or_insert(JsonValue::Object(Map::new()))
		.as_object_mut()
		.unwrap()
		.insert(name.to_owned(), pdu);

	self.unsigned = to_raw_value(&unsigned)
		.map(Some)
		.expect("unsigned is valid");

	Ok(())
}

#[implement(Pdu)]
pub fn contains_unsigned_property<F>(&self, property: &str, is_type: F) -> bool
where
	F: FnOnce(&JsonValue) -> bool,
{
	self.get_unsigned_as_value()
		.get(property)
		.map(is_type)
		.is_some_and(is_true!())
}

#[implement(Pdu)]
pub fn get_unsigned_property<T>(&self, property: &str) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	self.get_unsigned_as_value()
		.get_mut(property)
		.map(JsonValue::take)
		.map(serde_json::from_value)
		.ok_or(err!(Request(NotFound("property not found in unsigned object"))))?
		.map_err(|e| err!(Database("Failed to deserialize unsigned.{property} into type: {e}")))
}

#[implement(Pdu)]
#[must_use]
pub fn get_unsigned_as_value(&self) -> JsonValue {
	self.get_unsigned::<JsonValue>().unwrap_or_default()
}

#[implement(Pdu)]
pub fn get_unsigned<T>(&self) -> Result<JsonValue> {
	self.unsigned
		.as_ref()
		.map(|raw| raw.get())
		.map(serde_json::from_str)
		.ok_or(err!(Request(NotFound("\"unsigned\" property not found in pdu"))))?
		.map_err(|e| err!(Database("Failed to deserialize \"unsigned\" into value: {e}")))
}
