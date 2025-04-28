use std::{fmt, str::FromStr};

use ruma::{CanonicalJsonError, CanonicalJsonObject, canonical_json::try_from_json_map};

use crate::Result;

/// Fallible conversion from any value that implements `Serialize` to a
/// `CanonicalJsonObject`.
///
/// `value` must serialize to an `serde_json::Value::Object`.
pub fn to_canonical_object<T: serde::Serialize>(
	value: T,
) -> Result<CanonicalJsonObject, CanonicalJsonError> {
	use serde::ser::Error;

	match serde_json::to_value(value).map_err(CanonicalJsonError::SerDe)? {
		| serde_json::Value::Object(map) => try_from_json_map(map),
		| _ => {
			Err(CanonicalJsonError::SerDe(serde_json::Error::custom("Value must be an object")))
		},
	}
}

pub fn deserialize_from_str<
	'de,
	D: serde::de::Deserializer<'de>,
	T: FromStr<Err = E>,
	E: fmt::Display,
>(
	deserializer: D,
) -> Result<T, D::Error> {
	struct Visitor<T: FromStr<Err = E>, E>(std::marker::PhantomData<T>);
	impl<T: FromStr<Err = Err>, Err: fmt::Display> serde::de::Visitor<'_> for Visitor<T, Err> {
		type Value = T;

		fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
			write!(formatter, "a parsable string")
		}

		fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
		where
			E: serde::de::Error,
		{
			v.parse().map_err(serde::de::Error::custom)
		}
	}
	deserializer.deserialize_str(Visitor(std::marker::PhantomData))
}
