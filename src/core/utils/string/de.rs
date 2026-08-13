use std::fmt;

use serde::de::{Deserializer, Error, Visitor};

struct ToLowercase;

/// Deserializes a string and converts it to Unicode lowercase.
///
/// The adapter applies [`str::to_lowercase`] to the deserialized value. This is
/// ordinary lowercasing rather than full Unicode case folding.
#[inline]
pub fn to_lowercase<'de, D>(deserializer: D) -> Result<String, D::Error>
where
	D: Deserializer<'de>,
{
	deserializer.deserialize_string(ToLowercase)
}

impl Visitor<'_> for ToLowercase {
	type Value = String;

	#[inline]
	fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> { Ok(v.to_lowercase()) }

	fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("String") }
}
