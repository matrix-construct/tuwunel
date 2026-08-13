//! Key and value byte aliases plus database codec adapters.
//!
//! Raw aliases default to borrowed byte slices, while buffer aliases provide
//! inline capacity for common payload sizes. Serialization helpers use the
//! database record codec, and projection helpers consume pairs to return one
//! component without cloning or reserializing it.

use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, smallvec::SmallVec};

use crate::{de, ser};

/// Database key and value pair, borrowing raw byte slices by default.
///
/// `K` and `V` can replace the raw defaults with decoded or owned types. The
/// lifetime tracks borrowed forms and does not affect owned substitutions.
pub type KeyVal<'a, K = &'a Slice, V = &'a Slice> = (Key<'a, K>, Val<'a, V>);

/// Database key, borrowing a raw byte slice by default.
///
/// Substitute `T` for a decoded or owned key type. The alias adds no runtime
/// wrapper.
pub type Key<'a, T = &'a Slice> = T;

/// Database value, borrowing a raw byte slice by default.
///
/// Substitute `T` for a decoded or owned value type. The alias adds no runtime
/// wrapper.
pub type Val<'a, T = &'a Slice> = T;

/// Default inline-backed buffer for a serialized key.
///
/// This aliases [`KeyBuffer`] with [`KEY_STACK_CAP`] bytes of inline storage.
/// Longer keys remain valid and spill to the heap.
pub type KeyBuf = KeyBuffer;

/// Default inline-backed buffer for a serialized value.
///
/// This aliases [`ValBuffer`] with [`VAL_STACK_CAP`] bytes of inline storage.
/// Longer values remain valid and spill to the heap.
pub type ValBuf = ValBuffer;

/// Inline-backed key buffer with a configurable byte capacity.
///
/// The buffer provides `CAP` bytes of inline storage. Operations requiring
/// greater capacity spill it to the heap. [`KeyBuf`] selects the crate's
/// default key capacity.
pub type KeyBuffer<const CAP: usize = KEY_STACK_CAP> = Buffer<CAP>;

/// Inline-backed value buffer with a configurable byte capacity.
///
/// The buffer provides `CAP` bytes of inline storage. Operations requiring
/// greater capacity spill it to the heap. [`ValBuf`] selects the crate's
/// default value capacity.
pub type ValBuffer<const CAP: usize = VAL_STACK_CAP> = Buffer<CAP>;

/// Inline-backed byte buffer used by serialized keys and values.
///
/// The const parameter sets the number of bytes stored inline by [`SmallVec`].
/// Capacity growth beyond that budget uses heap storage without truncating the
/// payload.
pub type Buffer<const CAP: usize = DEF_STACK_CAP> = SmallVec<[Byte; CAP]>;

/// Unsized byte slice used by raw database APIs.
///
/// This aliases `[u8]` so borrowed keys and values share one canonical
/// spelling. The alias itself carries no encoding guarantee.
pub type Slice = [Byte];

/// Byte element used by raw database slices and buffers.
///
/// This aliases `u8` and gives the related storage aliases a common element
/// name. It carries no additional representation.
pub type Byte = u8;

/// Default inline byte capacity for serialized keys.
///
/// [`KeyBuffer`] uses this value when no capacity is supplied. Keys exceeding
/// the budget spill to the heap rather than being truncated.
pub const KEY_STACK_CAP: usize = 128 - 16;

/// Default inline byte capacity for serialized values.
///
/// [`ValBuffer`] uses this value when no capacity is supplied. Values exceeding
/// the budget spill to the heap rather than being truncated.
pub const VAL_STACK_CAP: usize = 512 - 16;

/// Default inline byte capacity for a generic [`Buffer`].
///
/// The generic default matches [`KEY_STACK_CAP`]. Key and value aliases may
/// select different capacities explicitly.
pub const DEF_STACK_CAP: usize = KEY_STACK_CAP;

/// Serializes a database key into an inline-backed key buffer.
///
/// The compact database record codec determines the byte representation. The
/// buffer spills to the heap if the encoded key exceeds [`KEY_STACK_CAP`].
///
/// # Panics
///
/// Panics if `T` requests a Serde data-model operation unsupported by the
/// database codec. Debug builds also panic when record-layout invariants are
/// violated or when a directly wrapped
/// `Json<Box<serde_json::value::RawValue>>` is serialized.
#[inline]
pub fn serialize_key<T>(val: T) -> Result<KeyBuf>
where
	T: Serialize,
{
	ser::serialize_to::<KeyBuf, _>(val)
}

/// Serializes a database value into an inline-backed value buffer.
///
/// The compact database record codec determines the byte representation. The
/// buffer spills to the heap if the encoded value exceeds [`VAL_STACK_CAP`].
///
/// # Panics
///
/// Panics if `T` requests a Serde data-model operation unsupported by the
/// database codec. Debug builds also panic when record-layout invariants are
/// violated or when a directly wrapped
/// `Json<Box<serde_json::value::RawValue>>` is serialized.
#[inline]
pub fn serialize_val<T>(val: T) -> Result<ValBuf>
where
	T: Serialize,
{
	ser::serialize_to::<ValBuf, _>(val)
}

/// Deserializes both components and treats any input or codec error as fatal.
///
/// The returned key and value may borrow from the original raw pair. Prefer the
/// fallible helper when a caller can propagate decoding failure.
///
/// # Panics
///
/// Panics if the input is an error or either component cannot be deserialized.
#[inline]
pub(crate) fn _expect_deserialize<'a, K, V>(kv: Result<KeyVal<'a>>) -> KeyVal<'a, K, V>
where
	K: Deserialize<'a>,
	V: Deserialize<'a>,
{
	result_deserialize(kv).expect("failed to deserialize result key/val")
}

/// Deserializes a key and treats any input or codec error as fatal.
///
/// The returned key may borrow from the original raw bytes. Prefer the fallible
/// helper when a caller can propagate decoding failure.
///
/// # Panics
///
/// Panics if the input is an error or the key cannot be deserialized.
#[inline]
pub(crate) fn _expect_deserialize_key<'a, K>(key: Result<Key<'a>>) -> Key<'a, K>
where
	K: Deserialize<'a>,
{
	result_deserialize_key(key).expect("failed to deserialize result key")
}

#[inline]
pub(crate) fn result_deserialize<'a, K, V>(kv: Result<KeyVal<'a>>) -> Result<KeyVal<'a, K, V>>
where
	K: Deserialize<'a>,
	V: Deserialize<'a>,
{
	deserialize(kv?)
}

#[inline]
pub(crate) fn result_deserialize_key<'a, K>(key: Result<Key<'a>>) -> Result<Key<'a, K>>
where
	K: Deserialize<'a>,
{
	deserialize_key(key?)
}

#[inline]
pub(crate) fn deserialize<'a, K, V>(kv: KeyVal<'a>) -> Result<KeyVal<'a, K, V>>
where
	K: Deserialize<'a>,
	V: Deserialize<'a>,
{
	Ok((deserialize_key::<K>(kv.0)?, deserialize_val::<V>(kv.1)?))
}

#[inline]
pub(crate) fn deserialize_key<'a, K>(key: Key<'a>) -> Result<Key<'a, K>>
where
	K: Deserialize<'a>,
{
	de::from_slice::<K>(key)
}

#[inline]
pub(crate) fn deserialize_val<'a, V>(val: Val<'a>) -> Result<Val<'a, V>>
where
	V: Deserialize<'a>,
{
	de::from_slice::<V>(val)
}

/// Returns the key component of a database pair.
///
/// The pair is consumed and its value component is dropped. No serialization or
/// allocation is performed by this projection.
#[inline]
pub fn key<K, V>(kv: KeyVal<'_, K, V>) -> Key<'_, K> { kv.0 }

/// Returns the value component of a database pair.
///
/// The pair is consumed and its key component is dropped. No serialization or
/// allocation is performed by this projection.
#[inline]
pub fn val<K, V>(kv: KeyVal<'_, K, V>) -> Val<'_, V> { kv.1 }
