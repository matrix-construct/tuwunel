use std::{collections::BTreeMap, fmt, marker::PhantomData};

use serde::{
	Deserializer, de,
	de::{MapAccess, SeqAccess},
};

struct Visitor(PhantomData<IdentityProviders>);

type IdentityProviders = BTreeMap<String, super::IdentityProvider>;

pub(super) fn deserialize<'de, D>(de: D) -> Result<IdentityProviders, D::Error>
where
	D: Deserializer<'de>,
{
	de.deserialize_any(Visitor(PhantomData))
}

impl<'de> de::Visitor<'de> for Visitor {
	type Value = IdentityProviders;

	fn expecting(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt.write_str("Mapping or Sequence")
	}

	fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
		let mut ret = Self::Value::new();
		while let Some((k, v)) = map.next_entry()? {
			ret.insert(k, v);
		}

		Ok(ret)
	}

	fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
		let mut ret = Self::Value::new();
		while let Some(v) = seq.next_element()? {
			ret.insert(ret.len().to_string(), v);
		}

		Ok(ret)
	}
}
