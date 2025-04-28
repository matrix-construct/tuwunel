use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, smallvec::SmallVec};

use crate::{de, ser};

pub type KeyVal<'a, K = &'a Slice, V = &'a Slice> = (Key<'a, K>, Val<'a, V>);
pub type Key<'a, T = &'a Slice> = T;
pub type Val<'a, T = &'a Slice> = T;

pub type KeyBuf = KeyBuffer;
pub type ValBuf = ValBuffer;

pub type KeyBuffer<const CAP: usize = KEY_STACK_CAP> = Buffer<CAP>;
pub type ValBuffer<const CAP: usize = VAL_STACK_CAP> = Buffer<CAP>;
pub type Buffer<const CAP: usize = DEF_STACK_CAP> = SmallVec<[Byte; CAP]>;

pub type Slice = [Byte];
pub type Byte = u8;

pub const KEY_STACK_CAP: usize = 128;
pub const VAL_STACK_CAP: usize = 512;
pub const DEF_STACK_CAP: usize = KEY_STACK_CAP;

#[inline]
pub fn serialize_key<T>(val: T) -> Result<KeyBuf>
where
	T: Serialize,
{
	ser::serialize_to::<KeyBuf, _>(val)
}

#[inline]
pub fn serialize_val<T>(val: T) -> Result<ValBuf>
where
	T: Serialize,
{
	ser::serialize_to::<ValBuf, _>(val)
}

#[inline]
pub(crate) fn _expect_deserialize<'a, K, V>(kv: Result<KeyVal<'a>>) -> KeyVal<'a, K, V>
where
	K: Deserialize<'a>,
	V: Deserialize<'a>,
{
	result_deserialize(kv).expect("failed to deserialize result key/val")
}

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

#[inline]
pub fn key<K, V>(kv: KeyVal<'_, K, V>) -> Key<'_, K> {
	kv.0
}

#[inline]
pub fn val<K, V>(kv: KeyVal<'_, K, V>) -> Val<'_, V> {
	kv.1
}
