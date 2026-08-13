//! Typed decoding adapters for stored database values.
//!
//! The extension trait composes value deserialization with query results and
//! follow-up mapping. Implementations preserve input errors and decode only
//! successful raw values.

use std::convert::identity;

use serde::Deserialize;
use tuwunel_core::Result;

/// Converts a stored database value into a typed Serde value.
///
/// Implementations adapt pinned value handles and their result wrappers to the
/// database decoder. Use [`Deserialized::map_de`] to transform the decoded
/// value or [`Deserialized::deserialized`] to return it directly.
pub trait Deserialized {
	/// Deserializes an intermediate value and maps it into the requested
	/// output.
	///
	/// The mapping closure runs only after successful deserialization. Any
	/// input or decoding error is returned without invoking the closure.
	fn map_de<T, U, F>(self, f: F) -> Result<U>
	where
		F: FnOnce(T) -> U,
		T: for<'de> Deserialize<'de>;

	/// Deserializes the stored value directly into the requested type.
	///
	/// This is equivalent to mapping with the identity function. Any input or
	/// decoding error is returned unchanged.
	#[inline]
	fn deserialized<T>(self) -> Result<T>
	where
		T: for<'de> Deserialize<'de>,
		Self: Sized,
	{
		self.map_de(identity::<T>)
	}
}
