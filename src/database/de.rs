use serde::{
	Deserialize, de,
	de::{DeserializeSeed, Visitor},
};
use tuwunel_core::{
	Error, Result, arrayvec::ArrayVec, checked, debug::DebugInspect, err, unhandled,
	utils::string,
};

/// Deserialize into T from buffer.
#[cfg_attr(
	unabridged,
	tracing::instrument(
		name = "deserialize",
		level = "trace",
		skip_all,
		fields(len = %buf.len()),
	)
)]
pub(crate) fn from_slice<'a, T>(buf: &'a [u8]) -> Result<T>
where
	T: Deserialize<'a>,
{
	let mut deserializer = Deserializer { buf, pos: 0, rec: 0, seq: 0 };

	T::deserialize(&mut deserializer).debug_inspect(|_| {
		deserializer
			.finished()
			.expect("deserialization failed to consume trailing bytes");
	})
}

/// Deserialization state.
pub(crate) struct Deserializer<'de> {
	buf: &'de [u8],
	pos: usize,
	rec: usize,
	seq: usize,
}

/// Directive to ignore a record. This type can be used to skip deserialization
/// until the next separator is found.
#[derive(Debug, Deserialize)]
pub struct Ignore;

/// Directive to ignore all remaining records. This can be used in a sequence to
/// ignore the rest of the sequence.
#[derive(Debug, Deserialize)]
pub struct IgnoreAll;

impl<'de> Deserializer<'de> {
	const SEP: u8 = crate::ser::SEP;

	/// Determine if the input was fully consumed and error if bytes remaining.
	/// This is intended for debug assertions; not optimized for parsing logic.
	fn finished(&self) -> Result {
		let pos = self.pos;
		let len = self.buf.len();
		let parsed = &self.buf[0..pos];
		let unparsed = &self.buf[pos..];
		let remain = self.remaining()?;
		let trailing_sep = remain == 1 && unparsed[0] == Self::SEP;
		(remain == 0 || trailing_sep)
			.then_some(())
			.ok_or(err!(SerdeDe(
				"{remain} trailing of {len} bytes not deserialized.\n{parsed:?}\n{unparsed:?}",
			)))
	}

	/// Called at the start of arrays and tuples
	#[inline]
	fn sequence_start(&mut self, len: usize) {
		debug_assert!(self.seq == 0, "Nested sequences are not handled at this time");
		self.seq = len;
	}

	/// Consume the current record to ignore it. Inside a sequence the next
	/// record is skipped but at the top-level all records are skipped such that
	/// deserialization completes with self.finished() == Ok.
	#[inline]
	fn record_ignore(&mut self) {
		if self.seq > 0 {
			self.record_next();
		} else {
			self.record_ignore_all();
		}
	}

	/// Consume the current and all remaining records to ignore them. Similar to
	/// Ignore at the top-level, but it can be provided in a sequence to Ignore
	/// all remaining elements.
	#[inline]
	fn record_ignore_all(&mut self) { self.record_trail(); }

	/// Consume the current record. The position pointer is moved to the start
	/// of the next record. Slice of the current record is returned.
	#[inline]
	fn record_next(&mut self) -> &'de [u8] {
		self.buf[self.pos..]
			.split(|b| *b == Deserializer::SEP)
			.inspect(|record| self.inc_pos(record.len()))
			.next()
			.expect("remainder of buf even if SEP was not found")
	}

	/// Peek at the first byte of the current record. If all records were
	/// consumed None is returned instead.
	#[inline]
	fn record_peek_byte(&self) -> Option<u8> {
		let started = self.pos != 0 || self.rec > 0;
		let buf = &self.buf[self.pos..];
		debug_assert!(
			!started || buf[0] == Self::SEP,
			"Missing expected record separator at current position"
		);

		buf.get::<usize>(started.into()).copied()
	}

	/// Consume the record separator such that the position cleanly points to
	/// the start of the next record. (Case for some sequences)
	#[inline]
	fn record_start(&mut self) {
		let started = self.pos != 0 || self.rec > 0;
		let input_done = self.pos >= self.buf.len();
		let output_done = self.rec >= self.seq;
		let incomplete = input_done && !output_done;
		debug_assert!(
			!started || incomplete || self.buf.get(self.pos) == Some(&Self::SEP),
			"Missing expected record separator at current position"
		);

		let inc = started && !incomplete;
		self.inc_pos(inc.into());
		self.inc_rec(1);
	}

	/// Consume all remaining bytes, which may include record separators,
	/// returning a raw slice.
	#[inline]
	fn record_trail(&mut self) -> &'de [u8] {
		let record = &self.buf[self.pos..];
		self.inc_pos(record.len());
		record
	}

	/// Increment the position pointer.
	#[inline]
	#[cfg_attr(
		unabridged,
		tracing::instrument(
			level = "trace",
			skip(self),
			fields(
				len = self.buf.len(),
				rem = self.remaining().unwrap_or_default().saturating_sub(n),
			),
		)
	)]
	fn inc_pos(&mut self, n: usize) {
		self.pos = self.pos.saturating_add(n);
		debug_assert!(self.pos <= self.buf.len(), "pos out of range");
	}

	#[inline]
	fn inc_rec(&mut self, n: usize) { self.rec = self.rec.saturating_add(n); }

	/// Unconsumed input bytes.
	#[inline]
	fn remaining(&self) -> Result<usize> {
		let pos = self.pos;
		let len = self.buf.len();
		checked!(len - pos)
	}
}

impl<'a, 'de: 'a> de::Deserializer<'de> for &'a mut Deserializer<'de> {
	type Error = Error;

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		self.sequence_start(1);
		visitor.visit_seq(self)
	}

	#[cfg_attr(
		unabridged,
		tracing::instrument(level = "trace", skip(self, visitor))
	)]
	fn deserialize_tuple<V>(self, len: usize, visitor: V) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		self.sequence_start(len);
		visitor.visit_seq(self)
	}

	#[cfg_attr(
		unabridged,
		tracing::instrument(level = "trace", skip(self, visitor))
	)]
	fn deserialize_tuple_struct<V>(
		self,
		_name: &'static str,
		len: usize,
		visitor: V,
	) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		self.sequence_start(len);
		visitor.visit_seq(self)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_map<V>(self, visitor: V) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		let input = self.record_next();
		let mut d = serde_json::Deserializer::from_slice(input);
		d.deserialize_map(visitor).map_err(Into::into)
	}

	#[cfg_attr(
		unabridged,
		tracing::instrument(level = "trace", skip(self, visitor))
	)]
	fn deserialize_struct<V>(
		self,
		name: &'static str,
		fields: &'static [&'static str],
		visitor: V,
	) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		let input = self.record_next();
		let mut d = serde_json::Deserializer::from_slice(input);
		d.deserialize_struct(name, fields, visitor)
			.map_err(Into::into)
	}

	#[cfg_attr(
		unabridged,
		tracing::instrument(level = "trace", skip(self, visitor))
	)]
	fn deserialize_unit_struct<V>(self, name: &'static str, visitor: V) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		match name {
			| "Ignore" => self.record_ignore(),
			| "IgnoreAll" => self.record_ignore_all(),
			| _ => unhandled!("Unrecognized deserialization Directive {name:?}"),
		}

		visitor.visit_unit()
	}

	#[cfg_attr(
		unabridged,
		tracing::instrument(level = "trace", skip(self, visitor))
	)]
	fn deserialize_newtype_struct<V>(self, name: &'static str, visitor: V) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		match name {
			| "$serde_json::private::RawValue" => visitor.visit_map(self),
			| "Json" => visitor
				.visit_newtype_struct(&mut serde_json::Deserializer::from_slice(
					self.record_trail(),
				))
				.map_err(|e| Self::Error::SerdeDe(format!("{name}: {e}").into())),

			| "Cbor" => visitor
				.visit_newtype_struct(&mut minicbor_serde::Deserializer::new(self.record_trail()))
				.map_err(|e| Self::Error::SerdeDe(format!("{name}: {e}").into())),

			| _ => visitor.visit_newtype_struct(self),
		}
	}

	#[cfg_attr(
		unabridged,
		tracing::instrument(level = "trace", skip(self, _visitor))
	)]
	fn deserialize_enum<V>(
		self,
		_name: &'static str,
		_variants: &'static [&'static str],
		_visitor: V,
	) -> Result<V::Value>
	where
		V: Visitor<'de>,
	{
		unhandled!("deserialize Enum not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		if self
			.buf
			.get(self.pos)
			.is_none_or(|b| *b == Deserializer::SEP)
		{
			visitor.visit_none()
		} else {
			visitor.visit_some(self)
		}
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_bool<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize bool not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_i8<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize i8 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_i16<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize i16 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_i32<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize i32 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		const BYTES: usize = size_of::<i64>();

		let end = self.pos.saturating_add(BYTES).min(self.buf.len());
		let bytes: ArrayVec<u8, BYTES> = self.buf[self.pos..end].try_into()?;
		let bytes = bytes
			.into_inner()
			.map_err(|_| Self::Error::SerdeDe("i64 buffer underflow".into()))?;

		self.inc_pos(BYTES);
		visitor.visit_i64(i64::from_be_bytes(bytes))
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_u8<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!(
			"deserialize u8 not implemented; try dereferencing the Handle for [u8] access \
			 instead"
		)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_u16<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize u16 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_u32<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize u32 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		const BYTES: usize = size_of::<u64>();

		let end = self.pos.saturating_add(BYTES).min(self.buf.len());
		let bytes: ArrayVec<u8, BYTES> = self.buf[self.pos..end].try_into()?;
		let bytes = bytes
			.into_inner()
			.map_err(|_| Self::Error::SerdeDe("u64 buffer underflow".into()))?;

		self.inc_pos(BYTES);
		visitor.visit_u64(u64::from_be_bytes(bytes))
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_f32<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize f32 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_f64<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize f64 not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_char<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize char not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		let input = self.record_next();
		let out = deserialize_str(input)?;
		visitor.visit_borrowed_str(out)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		let input = self.record_next();
		let out = string::string_from_bytes(input)?;
		visitor.visit_string(out)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		let input = self.record_trail();
		visitor.visit_borrowed_bytes(input)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_byte_buf<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize Byte Buf not implemented")
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_unit<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize Unit not implemented")
	}

	// this only used for $serde_json::private::RawValue at this time; see MapAccess
	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		let input = "$serde_json::private::RawValue";
		visitor.visit_borrowed_str(input)
	}

	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	fn deserialize_ignored_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
		unhandled!("deserialize Ignored Any not implemented")
	}

	#[cfg_attr(
		unabridged,
		tracing::instrument(level = "trace", skip_all, fields(?self.buf))
	)]
	fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
		const TYPE_PRE_1_91: &str = "serde_json::value::de::<impl serde_core::de::Deserialize \
		                             for serde_json::value::Value>::deserialize::ValueVisitor";
		const TYPE: &str = "serde_json::value::de::<impl serde_core::de::Deserialize<'_> for \
		                    serde_json::value::Value>::deserialize::ValueVisitor";
		debug_assert!(
			matches!(tuwunel_core::debug::type_name::<V>(), TYPE | TYPE_PRE_1_91),
			"deserialize_any: type not expected {0}",
			tuwunel_core::debug::type_name::<V>()
		);

		match self.record_peek_byte() {
			| Some(b'{') => self.deserialize_map(visitor),
			| Some(b'[') => serde_json::Deserializer::from_slice(self.record_next())
				.deserialize_seq(visitor)
				.map_err(Into::into),

			| _ => self.deserialize_str(visitor),
		}
	}
}

impl<'a, 'de: 'a> de::SeqAccess<'de> for &'a mut Deserializer<'de> {
	type Error = Error;

	#[cfg_attr(
		unabridged,
		tracing::instrument(level = "trace", skip(self, seed))
	)]
	fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>>
	where
		T: DeserializeSeed<'de>,
	{
		// Finished parsing the input.
		let finished = self.pos >= self.buf.len();

		// Completely satisfied the output.
		let complete = self.rec >= self.seq;

		// Leave after reaching the end of both the input and output. Leaving before
		// reaching the end of the input trips the finished() assertion. Leaving before
		// reaching the end of the output causes a length expectation panic on type T
		// i.e. tuple of size X instead of Y and trailing defaulting elements will not
		// be possible.
		if finished && complete {
			return Ok(None);
		}

		self.record_start();
		seed.deserialize(&mut **self).map(Some)
	}
}

// this only used for $serde_json::private::RawValue at this time. our db
// schema doesn't have its own map format; we use json for that anyway
impl<'a, 'de: 'a> de::MapAccess<'de> for &'a mut Deserializer<'de> {
	type Error = Error;

	#[cfg_attr(
		unabridged,
		tracing::instrument(level = "trace", skip(self, seed))
	)]
	fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>>
	where
		K: DeserializeSeed<'de>,
	{
		seed.deserialize(&mut **self).map(Some)
	}

	#[cfg_attr(
		unabridged,
		tracing::instrument(level = "trace", skip(self, seed))
	)]
	fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value>
	where
		V: DeserializeSeed<'de>,
	{
		seed.deserialize(&mut **self)
	}
}

// activate when stable; too soon now
//#[cfg(debug_assertions)]
#[inline]
fn deserialize_str(input: &[u8]) -> Result<&str> { string::str_from_bytes(input) }

//#[cfg(not(debug_assertions))]
#[cfg(disable)]
#[inline]
fn deserialize_str(input: &[u8]) -> Result<&str> {
	// SAFETY: Strings were written by the serializer to the database. Assuming no
	// database corruption, the string will be valid. Database corruption is
	// detected via rocksdb checksums.
	unsafe { std::str::from_utf8_unchecked(input) }
}
