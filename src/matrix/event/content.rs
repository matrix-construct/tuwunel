use serde::Deserialize;
use serde_json::value::Value as JsonValue;
use tuwunel_core::{Result, err};

use super::Event;

#[inline]
#[must_use]
pub(super) fn as_value<E: Event>(event: &E) -> JsonValue {
	get(event).expect("Failed to represent Event content as JsonValue")
}

#[inline]
pub(super) fn get<T, E>(event: &E) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
	E: Event,
{
	serde_json::from_str(event.content().get())
		.map_err(|e| err!(Request(BadJson("Failed to deserialize content into type: {e}"))))
}
