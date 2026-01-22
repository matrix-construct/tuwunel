use std::ops::Deref;

use serde::{Deserialize, Deserializer, de};

use super::Unquote;
use crate::{Result, err};

/// Unquoted string which deserialized from a quoted string. Construction from a
/// &str is infallible such that the input can already be unquoted. Construction
/// from serde deserialization is fallible and the input must be quoted.
#[repr(transparent)]
pub struct Unquoted(str);

impl<'a> Unquoted {
	#[inline]
	#[must_use]
	pub fn as_str(&'a self) -> &'a str { &self.0 }
}

impl<'a, 'de: 'a> Deserialize<'de> for &'a Unquoted {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		let s = <&'a str>::deserialize(deserializer)?;
		s.is_quoted()
			.then_some(s)
			.ok_or(err!(SerdeDe("expected quoted string")))
			.map_err(de::Error::custom)
			.map(Into::into)
	}
}

impl<'a> From<&'a str> for &'a Unquoted {
	fn from(s: &'a str) -> &'a Unquoted {
		let s: &'a str = s.unquote_infallible();

		//SAFETY: This is a pattern I lifted from ruma-identifiers for strong-type strs
		// by wrapping in a tuple-struct.
		#[expect(clippy::transmute_ptr_to_ptr)]
		unsafe {
			std::mem::transmute(s)
		}
	}
}

impl Deref for Unquoted {
	type Target = str;

	fn deref(&self) -> &Self::Target { &self.0 }
}

impl<'a> AsRef<str> for &'a Unquoted {
	fn as_ref(&self) -> &'a str { &self.0 }
}
