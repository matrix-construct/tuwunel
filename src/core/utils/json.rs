use std::{fmt, marker::PhantomData, str::FromStr};

use ruma::{
	CanonicalJsonError, CanonicalJsonObject, canonical_json::try_from_json_map, serde::Raw,
};

use crate::Result;

/// Perform a round-trip through serde_json starting with a native type T and
/// ending with a Ruma `Raw<U>` which is usually just T.
pub fn to_raw<T: serde::Serialize, U>(input: T) -> Result<Raw<U>> {
	Ok(serde_json::from_value(serde_json::to_value(input)?)?)
}

/// Fallible conversion from any value that implements `Serialize` to a
/// `CanonicalJsonObject`.
///
/// `value` must serialize to an `serde_json::Value::Object`.
pub fn to_canonical_object<T: serde::Serialize>(
	value: T,
) -> Result<CanonicalJsonObject, CanonicalJsonError> {
	use CanonicalJsonError::SerDe;
	use serde::ser::Error;

	match serde_json::to_value(value).map_err(SerDe)? {
		| serde_json::Value::Object(map) => try_from_json_map(map),
		| _ => Err(SerDe(serde_json::Error::custom("Value must be an object"))),
	}
}

pub fn deserialize_from_str<'de, D, T, E>(deserializer: D) -> Result<T, D::Error>
where
	D: serde::de::Deserializer<'de>,
	T: FromStr<Err = E>,
	E: fmt::Display,
{
	struct Visitor<T: FromStr<Err = E>, E>(PhantomData<T>);

	impl<T, Err> serde::de::Visitor<'_> for Visitor<T, Err>
	where
		T: FromStr<Err = Err>,
		Err: fmt::Display,
	{
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

	deserializer.deserialize_str(Visitor(PhantomData))
}
