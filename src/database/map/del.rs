use std::{fmt::Debug, io::Write};

use serde::Serialize;
use tuwunel_core::{arrayvec::ArrayVec, implement};

use crate::{keyval::KeyBuf, ser};

#[implement(super::Map)]
#[inline]
pub fn del<K>(&self, key: K)
where
	K: Serialize + Debug,
{
	let mut buf = KeyBuf::new();
	self.bdel(key, &mut buf);
}

#[implement(super::Map)]
#[inline]
pub fn adel<const MAX: usize, K>(&self, key: K)
where
	K: Serialize + Debug,
{
	let mut buf = ArrayVec::<u8, MAX>::new();
	self.bdel(key, &mut buf);
}

#[implement(super::Map)]
#[tracing::instrument(skip(self, buf), level = "trace")]
pub fn bdel<K, B>(&self, key: K, buf: &mut B)
where
	K: Serialize + Debug,
	B: Write + AsRef<[u8]>,
{
	let key = ser::serialize(buf, key).expect("failed to serialize deletion key");
	self.remove(key);
}
