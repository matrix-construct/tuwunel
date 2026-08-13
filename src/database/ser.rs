//! Serialization for the database's compact record codec.
//!
//! Tuples place [`SEP`] between every pair of adjacent elements, while
//! sequences follow each element's separator state. Supported integer scalars
//! use big-endian bytes, and [`Json`] and [`Cbor`] delegate their payloads to
//! the corresponding self-describing format.

use std::{io::Write, mem::replace};

use serde::{Deserialize, Serialize, ser};
use tuwunel_core::{Error, Result, debug::type_name, err, result::DebugInspect, unhandled};

/// Serializes a value into an owned byte vector.
///
/// The database record codec determines the representation. Use [`Json`] or
/// [`Cbor`] to delegate the wrapped payload to one of those formats.
///
/// # Panics
///
/// Panics if `T` requests a Serde data-model operation unsupported by this
/// codec. Debug builds also panic when record-layout invariants are violated or
/// when a directly wrapped `Json<Box<serde_json::value::RawValue>>` is
/// serialized.
#[inline]
pub fn serialize_to_vec<T: Serialize>(val: T) -> Result<Vec<u8>> {
	serialize_to::<Vec<u8>, T>(val)
}

/// Serializes a value into a default-constructed output buffer.
///
/// The buffer must support byte writes and expose its completed contents as a
/// slice. The returned buffer retains any inline-storage behavior chosen by
/// `B`.
///
/// # Panics
///
/// Panics if `T` requests a Serde data-model operation unsupported by this
/// codec. Debug builds also panic when record-layout invariants are violated or
/// when a directly wrapped `Json<Box<serde_json::value::RawValue>>` is
/// serialized.
#[inline]
pub fn serialize_to<B, T>(val: T) -> Result<B>
where
	B: Default + Write + AsRef<[u8]>,
	T: Serialize,
{
	let mut buf = B::default();
	serialize(&mut buf, val)?;

	Ok(buf)
}

/// Serializes a value into a caller-provided output buffer.
///
/// Encoding is written through the writer without resetting it, and the
/// returned slice covers the full buffer exposed through `AsRef<[u8]>`. The
/// compact codec supports its database-oriented subset of the Serde data model.
///
/// # Panics
///
/// Panics if `T` requests a Serde data-model operation unsupported by this
/// codec. Debug builds also panic when record-layout invariants are violated or
/// when a directly wrapped `Json<Box<serde_json::value::RawValue>>` is
/// serialized.
#[inline]
#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
pub fn serialize<'a, W, T>(out: &'a mut W, val: T) -> Result<&'a [u8]>
where
	W: Write + AsRef<[u8]> + 'a,
	T: Serialize,
{
	let mut serializer = Serializer { out, depth: 0, sep: false, fin: false };

	val.serialize(&mut serializer)
		.map_err(|error| err!(SerdeSer("{error}")))
		.debug_inspect(|()| {
			debug_assert_eq!(
				serializer.depth, 0,
				"Serialization completed at non-zero recursion level"
			);
		})?;

	Ok((*out).as_ref())
}

/// Stateful writer for the compact database record format.
///
/// Separator state controls record boundaries in the compact encoding. Debug
/// builds track container depth and explicit prefix finalization to validate
/// the layout.
pub(crate) struct Serializer<'a, W: Write> {
	out: &'a mut W,
	depth: u32,
	sep: bool,
	fin: bool,
}

/// Wraps a value for JSON encoding within the database codec.
///
/// Encoding and decoding delegate the wrapped value to `serde_json` instead of
/// the compact record rules. The wrapper changes the representation selected
/// for its inner value.
#[derive(Debug, Deserialize, Serialize)]
pub struct Json<T>(
	/// Wrapped value encoded as JSON.
	///
	/// The field remains public for direct construction and extraction.
	pub T,
);

/// Wraps a value for CBOR encoding within the database codec.
///
/// Encoding and decoding delegate the wrapped value to `minicbor_serde` instead
/// of the compact record rules. The wrapper changes the representation selected
/// for its inner value. Values containing Ruma `Raw<T>` fields do not
/// round-trip through this format; use [`Json`] for those values.
#[derive(Debug, Deserialize, Serialize)]
pub struct Cbor<T>(
	/// Wrapped value encoded as CBOR.
	///
	/// The field remains public for direct construction and extraction.
	pub T,
);

/// Finalizes a tuple prefix immediately after its trailing separator.
///
/// Place this zero-width marker as the final tuple element to encode a raw
/// prefix ending in [`SEP`]. It emits no payload of its own, and debug builds
/// reject serialization after it.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Interfix;

/// Emits one record separator explicitly.
///
/// Use this zero-width marker where a format requires [`SEP`] independently of
/// automatic container boundaries. Unlike [`Interfix`], it does not finalize
/// the surrounding value.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Separator;

/// Byte separating records in the compact database format.
///
/// The value is intentionally invalid UTF-8, so it cannot occur inside a valid
/// encoded string. Container state emits it at automatic record boundaries,
/// while [`Separator`] emits it explicitly.
pub const SEP: u8 = b'\xFF';

impl<W: Write> Serializer<'_, W> {
	const SEP: &'static [u8] = &[SEP];

	fn tuple_start(&mut self) {
		debug_assert!(!self.sep, "Tuple start with separator set");
		self.sequence_start();
	}

	fn tuple_end(&mut self) -> Result {
		self.sequence_end()?;
		Ok(())
	}

	fn sequence_start(&mut self) {
		debug_assert!(!self.is_finalized(), "Sequence start with finalization set");
		cfg!(debug_assertions).then(|| self.depth = self.depth.saturating_add(1));
		self.sep = false;
	}

	fn sequence_end(&mut self) -> Result {
		cfg!(debug_assertions).then(|| self.depth = self.depth.saturating_sub(1));
		self.sep = false;
		Ok(())
	}

	fn record_start(&mut self) -> Result {
		debug_assert!(!self.is_finalized(), "Starting a record after serialization finalized");
		replace(&mut self.sep, true)
			.then(|| self.separator())
			.unwrap_or(Ok(()))
	}

	fn separator(&mut self) -> Result {
		debug_assert!(!self.is_finalized(), "Writing a separator after serialization finalized");
		self.out.write_all(Self::SEP).map_err(Into::into)
	}

	fn write(&mut self, buf: &[u8]) -> Result { self.out.write_all(buf).map_err(Into::into) }

	fn set_finalized(&mut self) {
		debug_assert!(!self.is_finalized(), "Finalization already set");
		cfg!(debug_assertions).then(|| self.fin = true);
	}

	fn is_finalized(&self) -> bool { self.fin }
}

impl<W: Write> ser::Serializer for &mut Serializer<'_, W> {
	type Error = Error;
	type Ok = ();
	type SerializeMap = Self;
	type SerializeSeq = Self;
	type SerializeStruct = Self;
	type SerializeStructVariant = Self;
	type SerializeTuple = Self;
	type SerializeTupleStruct = Self;
	type SerializeTupleVariant = Self;

	fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq> {
		self.sequence_start();
		Ok(self)
	}

	fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple> {
		self.tuple_start();
		Ok(self)
	}

	fn serialize_tuple_struct(
		self,
		_name: &'static str,
		_len: usize,
	) -> Result<Self::SerializeTupleStruct> {
		self.tuple_start();
		Ok(self)
	}

	fn serialize_tuple_variant(
		self,
		_name: &'static str,
		_idx: u32,
		_var: &'static str,
		_len: usize,
	) -> Result<Self::SerializeTupleVariant> {
		unhandled!("serialize Tuple Variant not implemented")
	}

	fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap> {
		unhandled!(
			"serialize Map not implemented; did you mean to use database::Json() around your \
			 serde_json::Value?"
		)
	}

	fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Self::SerializeStruct> {
		unhandled!(
			"serialize Struct not implemented at this time; did you mean to use \
			 database::Json() around your struct?"
		)
	}

	fn serialize_struct_variant(
		self,
		_name: &'static str,
		_idx: u32,
		_var: &'static str,
		_len: usize,
	) -> Result<Self::SerializeStructVariant> {
		unhandled!("serialize Struct Variant not implemented")
	}

	fn serialize_newtype_struct<T>(self, name: &'static str, value: &T) -> Result<Self::Ok>
	where
		T: Serialize + ?Sized,
	{
		debug_assert!(
			name != "Json" || type_name::<T>() != "alloc::boxed::Box<serde_json::raw::RawValue>",
			"serializing a Json(RawValue); you can skip serialization instead"
		);

		match name {
			| "Json" => serde_json::to_writer(&mut *self.out, value).map_err(Into::into),
			| "Cbor" => {
				use minicbor::encode::write::Writer;
				use minicbor_serde::Serializer;

				value
					.serialize(&mut Serializer::new(&mut Writer::new(&mut *self.out)))
					.map_err(|e| Self::Error::SerdeSer(e.to_string().into()))
			},
			| _ => unhandled!("Unrecognized serialization Newtype {name:?}"),
		}
	}

	fn serialize_newtype_variant<T: Serialize + ?Sized>(
		self,
		_name: &'static str,
		_idx: u32,
		_var: &'static str,
		_value: &T,
	) -> Result<Self::Ok> {
		unhandled!("serialize Newtype Variant not implemented")
	}

	fn serialize_unit_struct(self, name: &'static str) -> Result<Self::Ok> {
		match name {
			| "Interfix" => {
				self.set_finalized();
			},
			| "Separator" => {
				self.separator()?;
			},
			| _ => unhandled!("Unrecognized serialization directive: {name:?}"),
		}

		Ok(())
	}

	fn serialize_unit_variant(
		self,
		_name: &'static str,
		_idx: u32,
		_var: &'static str,
	) -> Result<Self::Ok> {
		unhandled!("serialize Unit Variant not implemented")
	}

	fn serialize_some<T: Serialize + ?Sized>(self, val: &T) -> Result<Self::Ok> {
		val.serialize(self)
	}

	fn serialize_none(self) -> Result<Self::Ok> { Ok(()) }

	fn serialize_char(self, v: char) -> Result<Self::Ok> {
		let mut buf: [u8; 4] = [0; 4];
		self.serialize_str(v.encode_utf8(&mut buf))
	}

	fn serialize_str(self, v: &str) -> Result<Self::Ok> {
		debug_assert!(
			self.depth > 0,
			"serializing string at the top-level; you can skip serialization instead"
		);

		self.serialize_bytes(v.as_bytes())
	}

	fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok> {
		debug_assert!(
			self.depth > 0,
			"serializing byte array at the top-level; you can skip serialization instead"
		);

		self.write(v).inspect(|()| self.sep = true)
	}

	fn serialize_f64(self, _v: f64) -> Result<Self::Ok> {
		unhandled!("serialize f64 not implemented")
	}

	fn serialize_f32(self, _v: f32) -> Result<Self::Ok> {
		unhandled!("serialize f32 not implemented")
	}

	fn serialize_i64(self, v: i64) -> Result<Self::Ok> {
		self.write(&v.to_be_bytes())
			.inspect(|()| self.sep = false)
	}

	fn serialize_i32(self, v: i32) -> Result<Self::Ok> {
		self.write(&v.to_be_bytes())
			.inspect(|()| self.sep = false)
	}

	fn serialize_i16(self, _v: i16) -> Result<Self::Ok> {
		unhandled!("serialize i16 not implemented")
	}

	fn serialize_i8(self, _v: i8) -> Result<Self::Ok> {
		unhandled!("serialize i8 not implemented")
	}

	fn serialize_u64(self, v: u64) -> Result<Self::Ok> {
		self.write(&v.to_be_bytes())
			.inspect(|()| self.sep = false)
	}

	fn serialize_u32(self, v: u32) -> Result<Self::Ok> {
		self.write(&v.to_be_bytes())
			.inspect(|()| self.sep = false)
	}

	fn serialize_u16(self, _v: u16) -> Result<Self::Ok> {
		unhandled!("serialize u16 not implemented")
	}

	fn serialize_u8(self, v: u8) -> Result<Self::Ok> {
		self.write(&[v]).inspect(|()| self.sep = false)
	}

	fn serialize_bool(self, _v: bool) -> Result<Self::Ok> {
		unhandled!("serialize bool not implemented")
	}

	fn serialize_unit(self) -> Result<Self::Ok> { unhandled!("serialize unit not implemented") }
}

impl<W: Write> ser::SerializeSeq for &mut Serializer<'_, W> {
	type Error = Error;
	type Ok = ();

	fn serialize_element<T: Serialize + ?Sized>(&mut self, val: &T) -> Result<Self::Ok> {
		self.record_start()?;
		val.serialize(&mut **self)
	}

	fn end(self) -> Result<Self::Ok> { self.sequence_end() }
}

impl<W: Write> ser::SerializeTuple for &mut Serializer<'_, W> {
	type Error = Error;
	type Ok = ();

	fn serialize_element<T: Serialize + ?Sized>(&mut self, val: &T) -> Result<Self::Ok> {
		self.record_start()?;
		val.serialize(&mut **self)
			.inspect(|()| self.sep = true)
	}

	fn end(self) -> Result<Self::Ok> { self.tuple_end() }
}

impl<W: Write> ser::SerializeTupleStruct for &mut Serializer<'_, W> {
	type Error = Error;
	type Ok = ();

	fn serialize_field<T: Serialize + ?Sized>(&mut self, val: &T) -> Result<Self::Ok> {
		self.record_start()?;
		val.serialize(&mut **self)
			.inspect(|()| self.sep = true)
	}

	fn end(self) -> Result<Self::Ok> { self.tuple_end() }
}

impl<W: Write> ser::SerializeTupleVariant for &mut Serializer<'_, W> {
	type Error = Error;
	type Ok = ();

	fn serialize_field<T: Serialize + ?Sized>(&mut self, val: &T) -> Result<Self::Ok> {
		self.record_start()?;
		val.serialize(&mut **self)
			.inspect(|()| self.sep = true)
	}

	fn end(self) -> Result<Self::Ok> { self.tuple_end() }
}

impl<W: Write> ser::SerializeMap for &mut Serializer<'_, W> {
	type Error = Error;
	type Ok = ();

	fn serialize_key<T: Serialize + ?Sized>(&mut self, _key: &T) -> Result<Self::Ok> {
		unhandled!("serialize Map Key not implemented")
	}

	fn serialize_value<T: Serialize + ?Sized>(&mut self, _val: &T) -> Result<Self::Ok> {
		unhandled!("serialize Map Val not implemented")
	}

	fn end(self) -> Result<Self::Ok> { unhandled!("serialize Map End not implemented") }
}

impl<W: Write> ser::SerializeStruct for &mut Serializer<'_, W> {
	type Error = Error;
	type Ok = ();

	fn serialize_field<T: Serialize + ?Sized>(
		&mut self,
		_key: &'static str,
		_val: &T,
	) -> Result<Self::Ok> {
		unhandled!("serialize Struct Field not implemented")
	}

	fn end(self) -> Result<Self::Ok> { unhandled!("serialize Struct End not implemented") }
}

impl<W: Write> ser::SerializeStructVariant for &mut Serializer<'_, W> {
	type Error = Error;
	type Ok = ();

	fn serialize_field<T: Serialize + ?Sized>(
		&mut self,
		_key: &'static str,
		_val: &T,
	) -> Result<Self::Ok> {
		unhandled!("serialize Struct Variant Field not implemented")
	}

	fn end(self) -> Result<Self::Ok> {
		unhandled!("serialize Struct Variant End not implemented")
	}
}
