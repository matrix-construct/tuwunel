use serde::Deserialize;
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue};
use tuwunel_core::{Result, err, is_true};

use super::Event;

pub(super) fn contains_unsigned_property<F, E>(event: &E, property: &str, is_type: F) -> bool
where
	F: FnOnce(&JsonValue) -> bool,
	E: Event,
{
	get_unsigned_as_value(event)
		.get(property)
		.map(is_type)
		.is_some_and(is_true!())
}

pub(super) fn get_unsigned_property<T, E>(event: &E, property: &str) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
	E: Event,
{
	get_unsigned_as_value(event)
		.get_mut(property)
		.map(JsonValue::take)
		.map(serde_json::from_value)
		.ok_or(err!(Request(NotFound("property not found in unsigned object"))))?
		.map_err(|e| err!(Database("Failed to deserialize unsigned.{property} into type: {e}")))
}

#[must_use]
pub(super) fn get_unsigned_as_value<E>(event: &E) -> JsonValue
where
	E: Event,
{
	get_unsigned::<JsonValue, E>(event).unwrap_or_default()
}

pub(super) fn get_unsigned<T, E>(event: &E) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
	E: Event,
{
	event
		.unsigned()
		.map(RawJsonValue::get)
		.map(serde_json::from_str)
		.ok_or(err!(Request(NotFound("\"unsigned\" property not found in pdu"))))?
		.map_err(|e| err!(Database("Failed to deserialize \"unsigned\" into value: {e}")))
}
