#![allow(unused_features)] // 1.96.0-nightly 2026-03-07 bug
#![expect(clippy::needless_borrows_for_generic_args)]

use std::{fmt::Debug, process::id as process_id, sync::Arc};

use rocksdb::WriteBatch;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tracing::subscriber::NoSubscriber;
use tuwunel_core::{
	Result, Server,
	arrayvec::ArrayVec,
	config::{Config, Figment},
	log::{LogLevelReloadHandles, Logging, capture::State},
	metrics::Metrics,
	ruma::{EventId, RoomId, UserId, serde::Raw},
};

use crate::{
	Cbor, Database, Ignore, Interfix,
	de::from_slice,
	ser,
	ser::{Json, serialize_to_vec},
	txn::next_record,
};

#[test]
#[cfg_attr(
	debug_assertions,
	should_panic(expected = "serializing string at the top-level")
)]
fn ser_str() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let s = serialize_to_vec(&user_id).expect("failed to serialize user_id");
	assert_eq!(&s, user_id.as_bytes());
}

#[test]
fn ser_tuple() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();

	let mut a = user_id.as_bytes().to_vec();
	a.push(0xFF);
	a.extend_from_slice(room_id.as_bytes());

	let b = (user_id, room_id);
	let b = serialize_to_vec(&b).expect("failed to serialize tuple");

	assert_eq!(a, b);
}

#[test]
fn ser_tuple_option() {
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let mut a = Vec::<u8>::new();
	a.push(0xFF);
	a.extend_from_slice(user_id.as_bytes());

	let mut aa = Vec::<u8>::new();
	aa.extend_from_slice(room_id.as_bytes());
	aa.push(0xFF);
	aa.extend_from_slice(user_id.as_bytes());

	let b: (Option<&RoomId>, &UserId) = (None, user_id);
	let b = serialize_to_vec(&b).expect("failed to serialize tuple");
	assert_eq!(a, b);

	let bb: (Option<&RoomId>, &UserId) = (Some(room_id), user_id);
	let bb = serialize_to_vec(&bb).expect("failed to serialize tuple");
	assert_eq!(aa, bb);
}

#[test]
#[should_panic(expected = "I/O error: failed to write whole buffer")]
fn ser_overflow() {
	const BUFSIZE: usize = 10;

	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();

	assert!(BUFSIZE < user_id.as_str().len() + room_id.as_str().len());
	let mut buf = ArrayVec::<u8, BUFSIZE>::new();

	let val = (user_id, room_id);
	_ = ser::serialize(&mut buf, val).unwrap();
}

#[test]
fn ser_complex() {
	use tuwunel_core::ruma::Mxc;

	#[derive(Debug, Serialize)]
	struct Dim {
		width: u32,
		height: u32,
	}

	let mxc = Mxc {
		server_name: "example.com".try_into().unwrap(),
		media_id: "AbCdEfGhIjK",
	};

	let dim = Dim { width: 123, height: 456 };

	let mut a = Vec::new();
	a.extend_from_slice(b"mxc://");
	a.extend_from_slice(mxc.server_name.as_bytes());
	a.extend_from_slice(b"/");
	a.extend_from_slice(mxc.media_id.as_bytes());
	a.push(0xFF);
	a.extend_from_slice(&dim.width.to_be_bytes());
	a.extend_from_slice(&dim.height.to_be_bytes());
	a.push(0xFF);

	let d: &[u32] = &[dim.width, dim.height];
	let b = (mxc, d, Interfix);
	let b = serialize_to_vec(b).expect("failed to serialize complex");

	assert_eq!(a, b);
}

#[test]
fn ser_json() {
	use tuwunel_core::ruma::api::client::filter::FilterDefinition;

	let filter = FilterDefinition {
		event_fields: Some(vec!["content.body".to_owned()]),
		..Default::default()
	};

	let serialized = serialize_to_vec(Json(&filter)).expect("failed to serialize value");

	let s = String::from_utf8_lossy(&serialized);
	assert_eq!(&s, r#"{"event_fields":["content.body"]}"#);
}

#[test]
fn ser_json_value() {
	use tuwunel_core::ruma::api::client::filter::FilterDefinition;

	let filter = FilterDefinition {
		event_fields: Some(vec!["content.body".to_owned()]),
		..Default::default()
	};

	let value = serde_json::to_value(filter).expect("failed to serialize to serde_json::value");
	let serialized = serialize_to_vec(Json(value)).expect("failed to serialize value");

	let s = String::from_utf8_lossy(&serialized);
	assert_eq!(&s, r#"{"event_fields":["content.body"]}"#);
}

#[test]
fn ser_json_macro() {
	use serde_json::json;

	#[derive(Serialize)]
	struct Foo {
		foo: String,
	}

	let content = Foo { foo: "bar".to_owned() };
	let content = serde_json::to_value(content).expect("failed to serialize content");
	let sender: &UserId = "@foo:example.com".try_into().unwrap();
	let serialized = serialize_to_vec(Json(json!({
		"content": content,
		"sender": sender,
	})))
	.expect("failed to serialize value");

	let s = String::from_utf8_lossy(&serialized);
	assert_eq!(&s, r#"{"content":{"foo":"bar"},"sender":"@foo:example.com"}"#);
}

#[test]
#[cfg_attr(
	debug_assertions,
	should_panic(expected = "serializing string at the top-level")
)]
fn ser_json_raw() {
	use tuwunel_core::ruma::api::client::filter::FilterDefinition;

	let filter = FilterDefinition {
		event_fields: Some(vec!["content.body".to_owned()]),
		..Default::default()
	};

	let value =
		serde_json::value::to_raw_value(&filter).expect("failed to serialize to raw value");
	let a = serialize_to_vec(value.get()).expect("failed to serialize raw value");
	let s = String::from_utf8_lossy(&a);
	assert_eq!(&s, r#"{"event_fields":["content.body"]}"#);
}

#[test]
#[cfg_attr(
	debug_assertions,
	should_panic(expected = "you can skip serialization instead")
)]
fn ser_json_raw_json() {
	use tuwunel_core::ruma::api::client::filter::FilterDefinition;

	let filter = FilterDefinition {
		event_fields: Some(vec!["content.body".to_owned()]),
		..Default::default()
	};

	let value =
		serde_json::value::to_raw_value(&filter).expect("failed to serialize to raw value");
	let a = serialize_to_vec(Json(value)).expect("failed to serialize json value");
	let s = String::from_utf8_lossy(&a);
	assert_eq!(&s, r#"{"event_fields":["content.body"]}"#);
}

#[test]
fn ser_cbor() {
	use tuwunel_core::ruma::api::client::filter::FilterDefinition;

	let filter = FilterDefinition {
		event_fields: Some(vec!["content.body".to_owned()]),
		..Default::default()
	};

	let serialized = serialize_to_vec(Cbor(&filter)).expect("failed to serialize cbor");
	let deserialized: FilterDefinition = from_slice::<Cbor<_>>(&serialized)
		.expect("failed to deserialize cbor")
		.0;

	assert_eq!(filter.event_fields, deserialized.event_fields);
}

#[test]
#[cfg(disable)]
fn ser_cbor_ruma_raw() {
	use serde_json::value::RawValue;
	use tuwunel_core::ruma::api::client::filter::FilterDefinition;

	struct Foo {
		a: String,
		b: Box<RawValue>,
	}

	let filter = FilterDefinition {
		event_fields: Some(vec!["content.body".to_owned()]),
		..Default::default()
	};

	let foo = Foo {
		a: "test".into(),
		b: serde_json::value::to_raw_value(&filter).expect("failed to serialize to raw value"),
	};

	let serialized = serialize_to_vec(Cbor(&foo)).expect("failed to serialize cbor");
	let deserialized: Foo = from_slice::<Cbor<_>>(&serialized)
		.expect("failed to deserialize cbor")
		.0;

	assert_eq!(foo.a, deserialized.a);
	assert_eq!(foo.a.get(), deserialized.b.get());
}

/// `Raw<T>` does NOT round-trip through `Cbor`: serialization succeeds but
/// the value is encoded as a CBOR newtype struct that the JSON-flavored
/// `RawValue` deserializer cannot consume. Use `Json(...)` (see
/// `ser_json_raw_field_roundtrip` below) when a value contains a `Raw<T>`
/// field.
#[test]
#[should_panic(expected = "expected any valid JSON value")]
fn ser_cbor_raw_field_roundtrip() {
	#[derive(Debug, Serialize, Deserialize)]
	struct Entry {
		key: Raw<serde_json::Value>,
		used: bool,
	}

	let entry = Entry {
		key: Raw::from_json_string(r#"{"hello":"world","n":42}"#.to_owned())
			.expect("construct Raw"),
		used: false,
	};

	let serialized = serialize_to_vec(Cbor(&entry)).expect("serialize cbor");

	let _: Entry = from_slice::<Cbor<_>>(&serialized)
		.expect("deserialize cbor")
		.0;
}

/// Round-trip the same `Raw<T>`-bearing struct through `Json`. This is the
/// supported path: `RawValue`'s special-case in `serde_json` preserves the
/// inner JSON bytes verbatim across both directions.
#[test]
fn ser_json_raw_field_roundtrip() {
	#[derive(Debug, Serialize, Deserialize)]
	struct Entry {
		key: Raw<serde_json::Value>,
		used: bool,
	}

	let original_json = r#"{"hello":"world","n":42}"#;
	let entry = Entry {
		key: Raw::from_json_string(original_json.to_owned()).expect("construct Raw"),
		used: false,
	};

	let serialized = serialize_to_vec(Json(&entry)).expect("serialize json");

	let deserialized: Entry = from_slice::<Json<_>>(&serialized)
		.expect("deserialize json")
		.0;

	assert_eq!(entry.used, deserialized.used);
	assert_eq!(
		entry.key.json().get(),
		deserialized.key.json().get(),
		"Raw<T> JSON did not round-trip through Json"
	);
}

#[test]
fn de_tuple() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com\xFF!room:example.com";
	let (a, b): (&UserId, &RoomId) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, room_id, "deserialized room_id does not match");
}

#[test]
#[should_panic(expected = "failed to deserialize")]
fn de_tuple_invalid() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com\xFF@user:example.com";
	let (a, b): (&UserId, &RoomId) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, room_id, "deserialized room_id does not match");
}

#[test]
#[should_panic(expected = "failed to deserialize")]
fn de_tuple_incomplete() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com";
	let (a, _): (&UserId, &RoomId) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
}

#[test]
fn de_tuple_incomplete_default() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com";
	let (a, b): (&UserId, &str) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, "", "deserialized defaulted str does not match");
}

#[test]
#[should_panic(expected = "failed to deserialize")]
fn de_tuple_incomplete_nodefault() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com";
	let (a, _): (&UserId, u64) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
}

#[test]
fn de_tuple_incomplete_option() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com";
	let (a, b): (&UserId, Option<&str>) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, None, "deserialized defaulted Option does not match");
}

#[test]
fn de_tuple_incomplete_str() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com";
	let (a, b): (&UserId, &str) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, "", "trailing &str defaulted from missing input");
}

#[test]
fn de_tuple_incomplete_bytes() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com";
	let (a, b): (&UserId, &[u8]) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert!(b.is_empty(), "trailing &[u8] defaulted from missing input");
}

#[test]
fn de_tuple_incomplete_str_after_sep() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com\xFF";
	let (a, b): (&UserId, &str) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, "", "trailing &str defaulted from empty record after sep");
}

#[test]
fn serde_tuple_additive_evolution() {
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();
	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let count: u64 = 42;

	let old_bytes =
		serialize_to_vec(&(room_id, count, user_id)).expect("failed to serialize old key");

	let (r, c, u, tail): (&RoomId, u64, &UserId, &str) =
		from_slice(&old_bytes).expect("failed to deserialize old key as new type");

	assert_eq!(r, room_id);
	assert_eq!(c, count);
	assert_eq!(u, user_id);
	assert_eq!(tail, "", "tail must default to empty for old rows");

	let new_bytes =
		serialize_to_vec(&(room_id, count, user_id, "")).expect("failed to serialize new key");

	assert_eq!(&new_bytes[..old_bytes.len()], &*old_bytes);
	assert_eq!(new_bytes.len(), old_bytes.len() + 1);
	assert_eq!(*new_bytes.last().unwrap(), 0xFF);

	let (r, c, u, tail): (&RoomId, u64, &UserId, &str) =
		from_slice(&new_bytes).expect("failed to deserialize new key");

	assert_eq!(r, room_id);
	assert_eq!(c, count);
	assert_eq!(u, user_id);
	assert_eq!(tail, "");
}

#[test]
fn serde_tuple_additive_evolution_option() {
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();
	let count: u64 = 42;

	let old_bytes = serialize_to_vec(&(room_id, count)).expect("failed to serialize old key");

	let (r, c, tail): (&RoomId, u64, Option<&UserId>) =
		from_slice(&old_bytes).expect("failed to deserialize");

	assert_eq!(r, room_id);
	assert_eq!(c, count);
	assert_eq!(tail, None);
}

#[test]
fn serde_tuple_additive_evolution_u64_option() {
	let count: u64 = 42;
	let ts: u64 = 1_700_000_000_000;

	// Old rows wrote a bare u64 count; reading as the evolved
	// (count, Option<ts>) tuple lands the tail at None.
	let old_bytes = serialize_to_vec(&count).expect("failed to serialize old value");

	let (c, tail): (u64, Option<u64>) =
		from_slice(&old_bytes).expect("failed to deserialize old value as new type");

	assert_eq!(c, count);
	assert_eq!(tail, None, "ts tail must default to None for old rows");

	// New rows write (count, ts); the fixed-width tail round-trips as Some
	// even though its leading big-endian byte is 0x00 (never the 0xFF separator).
	let new_bytes = serialize_to_vec(&(count, ts)).expect("failed to serialize new value");

	assert_eq!(&new_bytes[..old_bytes.len()], &*old_bytes);
	assert_eq!(new_bytes[old_bytes.len()], 0xFF, "separator precedes the ts tail");
	assert_eq!(new_bytes.len(), old_bytes.len() + 1 + size_of::<u64>());

	let (c, tail): (u64, Option<u64>) =
		from_slice(&new_bytes).expect("failed to deserialize new value");

	assert_eq!(c, count);
	assert_eq!(tail, Some(ts));
}

#[test]
fn ser_de_eventid_backoff_record() {
	let event_id: &EventId = "$evt:example.com".try_into().unwrap();
	let ctx: u8 = 2;
	let bucket: u32 = 0x2A2B_2C2D;

	// The production read path deserializes the value; it must round-trip.
	let val = serialize_to_vec(&(1_u64, 1_717_171_717_u64)).expect("failed to serialize value");
	let (class, secs): (u64, u64) = from_slice(&val).expect("failed to deserialize value");
	assert_eq!(class, 1, "class byte does not round-trip");
	assert_eq!(secs, 1_717_171_717, "timestamp does not round-trip");

	// The bucket key carries the (ctx, event_id) prefix the scan and delete use.
	let key = serialize_to_vec(&(ctx, event_id, bucket)).expect("failed to serialize key");
	let prefix =
		serialize_to_vec(&(ctx, event_id, Interfix)).expect("failed to serialize prefix");
	assert!(key.starts_with(&prefix), "bucket key does not carry its prefix");

	// The trailing separator bounds the scan: a longer id sharing a byte-prefix
	// must not match.
	let longer: &EventId = "$evt:example.computer".try_into().unwrap();
	let longer_key =
		serialize_to_vec(&(ctx, longer, bucket)).expect("failed to serialize longer");
	assert!(!longer_key.starts_with(&prefix), "prefix bleeds into a longer id");
}

#[test]
#[should_panic(expected = "failed to deserialize")]
fn de_tuple_incomplete_non_tolerant_tail() {
	let raw: &[u8] = b"@user:example.com";
	let _: (&UserId, u64) = from_slice(raw).expect("failed to deserialize");
}

#[test]
#[should_panic(expected = "failed to deserialize")]
fn de_tuple_incomplete_with_sep() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com\xFF";
	let (a, _): (&UserId, &RoomId) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
}

#[test]
#[cfg_attr(
	debug_assertions,
	should_panic(expected = "deserialization failed to consume trailing bytes")
)]
fn de_tuple_unfinished() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com\xFF!room:example.com\xFF@user:example.com";
	let (a, b): (&UserId, &RoomId) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, room_id, "deserialized room_id does not match");
}

#[test]
fn de_tuple_ignore() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com\xFF@user2:example.net\xFF!room:example.com";
	let (a, _, c): (&UserId, Ignore, &RoomId) = from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(c, room_id, "deserialized room_id does not match");
}

#[test]
fn de_json_array() {
	let a = &["foo", "bar", "baz"];
	let s = serde_json::to_vec(a).expect("failed to serialize to JSON array");

	let b: Raw<Vec<Raw<String>>> = from_slice(&s).expect("failed to deserialize");

	let d: Vec<String> =
		serde_json::from_str(b.json().get()).expect("failed to deserialize JSON");

	for (i, a) in a.iter().enumerate() {
		assert_eq!(*a, d[i]);
	}
}

#[test]
fn de_json_raw_array() {
	let a = &["foo", "bar", "baz"];
	let s = serde_json::to_vec(a).expect("failed to serialize to JSON array");

	let b: Raw<Vec<Raw<String>>> = from_slice(&s).expect("failed to deserialize");

	let c: Vec<Raw<String>> =
		serde_json::from_str(b.json().get()).expect("failed to deserialize JSON");

	for (i, a) in a.iter().enumerate() {
		let c = serde_json::to_value(c[i].json()).expect("failed to deserialize JSON to string");
		assert_eq!(*a, c);
	}
}

#[test]
fn ser_array_integer() {
	let a: u64 = 123_456;
	let b: u64 = 987_654;

	let arr: &[u64] = &[a, b];
	let vec: Vec<u64> = vec![a, b];
	let arv: ArrayVec<u64, 2> = [a, b].into();

	let mut v = Vec::new();
	v.extend_from_slice(&a.to_be_bytes());
	v.extend_from_slice(&b.to_be_bytes());

	let s = serialize_to_vec(arr).expect("failed to serialize");
	assert_eq!(&s, &v, "serialization does not match");

	let s = serialize_to_vec(arv.as_slice()).expect("failed to serialize arrayvec");
	assert_eq!(&s, &v, "arrayvec serialization does not match");

	let s = serialize_to_vec(&vec).expect("failed to serialize borrowed vec");
	assert_eq!(&s, &v, "borrowed vec serialization does not match");

	let s = serialize_to_vec(vec).expect("failed to serialize vec");
	assert_eq!(&s, &v, "vec serialization does not match");
}

#[test]
fn ser_array_string() {
	let a = "foo";
	let b = "bar";

	let arr_str: &[&str] = &[a, b];
	let arr_string: &[String] = &[a.to_owned(), b.to_owned()];
	let vec_str: Vec<&str> = vec![a, b];
	let vec_string: Vec<String> = vec![a.to_owned(), b.to_owned()];
	let arv_str: ArrayVec<&str, 2> = [a, b].into();
	let arv_string: ArrayVec<String, 2> = [a.to_owned(), b.to_owned()].into();

	let v = b"foo\xFFbar";

	let s = serialize_to_vec(arr_str).expect("failed to serialize arr_str");
	assert_eq!(&s, &v, "arr_str serialization does not match");

	let s = serialize_to_vec(arr_string).expect("failed to serialize arr_string");
	assert_eq!(&s, &v, "arr_string serialization does not match");

	let s = serialize_to_vec(vec_str).expect("failed to serialize vec_str");
	assert_eq!(&s, &v, "vec_str serialization does not match");

	let s = serialize_to_vec(vec_string).expect("failed to serialize vec_string");
	assert_eq!(&s, &v, "vec_string serialization does not match");

	let s = serialize_to_vec(arv_str).expect("failed to serialize arv_str");
	assert_eq!(&s, &v, "arv_str serialization does not match");

	let s = serialize_to_vec(arv_string).expect("failed to serialize arv_string");
	assert_eq!(&s, &v, "arv_string serialization does not match");
}

#[test]
fn ser_array_one_string() {
	let a = "foo";

	let arr_str: &[&str] = &[a];
	let arr_string: &[String] = &[a.to_owned()];
	let vec_str: Vec<&str> = vec![a];
	let vec_string: Vec<String> = vec![a.to_owned()];
	let arv_str: ArrayVec<&str, 1> = [a].into();
	let arv_string: ArrayVec<String, 1> = [a.to_owned()].into();

	let v = b"foo";

	let s = serialize_to_vec(arr_str).expect("failed to serialize arr_str");
	assert_eq!(&s, &v, "arr_str serialization does not match");

	let s = serialize_to_vec(arr_string).expect("failed to serialize arr_string");
	assert_eq!(&s, &v, "arr_string serialization does not match");

	let s = serialize_to_vec(vec_str).expect("failed to serialize vec_str");
	assert_eq!(&s, &v, "vec_str serialization does not match");

	let s = serialize_to_vec(vec_string).expect("failed to serialize vec_string");
	assert_eq!(&s, &v, "vec_string serialization does not match");

	let s = serialize_to_vec(arv_str).expect("failed to serialize arv_str");
	assert_eq!(&s, &v, "arv_str serialization does not match");

	let s = serialize_to_vec(arv_string).expect("failed to serialize arv_string");
	assert_eq!(&s, &v, "arv_string serialization does not match");
}

#[test]
#[ignore = "does not work yet. TODO! Fixme!"]
fn de_array_integer() {
	let a: u64 = 123_456;
	let b: u64 = 987_654;

	let mut v: Vec<u8> = Vec::new();
	v.extend_from_slice(&a.to_be_bytes());
	v.extend_from_slice(&b.to_be_bytes());

	let arv: ArrayVec<u64, 2> = from_slice::<ArrayVec<u64, 2>>(v.as_slice())
		.map(TryInto::try_into)
		.expect("failed to deserialize to arrayvec")
		.expect("failed to deserialize into");

	assert_eq!(arv[0], a, "deserialized arv [0] does not match");
	assert_eq!(arv[1], b, "deserialized arv [1] does not match");

	let arr: [u64; 2] = from_slice::<[u64; 2]>(v.as_slice())
		.map(TryInto::try_into)
		.expect("failed to deserialize to array")
		.expect("failed to deserialize into");

	assert_eq!(arr[0], a, "deserialized arr [0] does not match");
	assert_eq!(arr[1], b, "deserialized arr [1] does not match");

	let vec: Vec<u64> = from_slice(v.as_slice()).expect("failed to deserialize to vec");

	assert_eq!(vec[0], a, "deserialized vec [0] does not match");
	assert_eq!(vec[1], b, "deserialized vec [1] does not match");
}

#[test]
fn de_array_string() {
	let a = "foo";
	let b = "bar";
	let v = b"foo\xFFbar";

	let arv: ArrayVec<&str, 2> = from_slice::<ArrayVec<&str, 2>>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to arrayvec")
		.expect("failed to deserialize into");
	assert_eq!(arv[0], a, "deserialized arv [0] does not match");
	assert_eq!(arv[1], b, "deserialized arv [1] does not match");
	assert_eq!(arv.len(), 2);

	let arv: ArrayVec<String, 2> = from_slice::<ArrayVec<String, 2>>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to arrayvec")
		.expect("failed to deserialize into");
	assert_eq!(arv[0], a, "deserialized arv [0] does not match");
	assert_eq!(arv[1], b, "deserialized arv [1] does not match");
	assert_eq!(arv.len(), 2);

	let arr: [&str; 2] = from_slice::<[&str; 2]>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to array")
		.expect("failed to deserialize into");
	assert_eq!(arr[0], a, "deserialized arr [0] does not match");
	assert_eq!(arr[1], b, "deserialized arr [1] does not match");

	let arr: [String; 2] = from_slice::<[String; 2]>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to array")
		.expect("failed to deserialize into");
	assert_eq!(arr[0], a, "deserialized arr [0] does not match");
	assert_eq!(arr[1], b, "deserialized arr [1] does not match");

	let vec: Vec<&str> = from_slice(v).expect("failed to deserialize to vec");
	assert_eq!(vec[0], a, "deserialized vec [0] does not match");
	assert_eq!(vec[1], b, "deserialized vec [1] does not match");
	assert_eq!(vec.len(), 2);

	let vec: Vec<String> = from_slice(v).expect("failed to deserialize to vec");
	assert_eq!(vec[0], a, "deserialized vec [0] does not match");
	assert_eq!(vec[1], b, "deserialized vec [1] does not match");
	assert_eq!(vec.len(), 2);
}

#[test]
fn de_array_one_string() {
	let a = "foo";
	let v = b"foo";

	let arv: ArrayVec<&str, 1> = from_slice::<ArrayVec<&str, 1>>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to arrayvec")
		.expect("failed to deserialize into");
	assert_eq!(arv[0], a, "deserialized arv [0] does not match");
	assert_eq!(arv.len(), 1);

	let arv: ArrayVec<String, 1> = from_slice::<ArrayVec<String, 1>>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to arrayvec")
		.expect("failed to deserialize into");
	assert_eq!(arv[0], a, "deserialized arv [0] does not match");
	assert_eq!(arv.len(), 1);

	let arr: [&str; 1] = from_slice::<[&str; 1]>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to array")
		.expect("failed to deserialize into");
	assert_eq!(arr[0], a, "deserialized arr [0] does not match");

	let arr: [String; 1] = from_slice::<[String; 1]>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to array")
		.expect("failed to deserialize into");
	assert_eq!(arr[0], a, "deserialized arr [0] does not match");

	let vec: Vec<&str> = from_slice(v).expect("failed to deserialize to vec");
	assert_eq!(vec[0], a, "deserialized vec [0] does not match");
	assert_eq!(vec.len(), 1);

	let vec: Vec<String> = from_slice(v).expect("failed to deserialize to vec");
	assert_eq!(vec[0], a, "deserialized vec [0] does not match");
	assert_eq!(vec.len(), 1);
}

#[test]
#[ignore = "does not work yet. TODO! Fixme!"]
fn de_complex() {
	type Key<'a> = (&'a UserId, ArrayVec<u64, 2>, &'a RoomId);

	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();
	let a: u64 = 123_456;
	let b: u64 = 987_654;

	let mut v = Vec::new();
	v.extend_from_slice(user_id.as_bytes());
	v.extend_from_slice(b"\xFF");
	v.extend_from_slice(&a.to_be_bytes());
	v.extend_from_slice(&b.to_be_bytes());
	v.extend_from_slice(b"\xFF");
	v.extend_from_slice(room_id.as_bytes());

	let arr: &[u64] = &[a, b];
	let key = (user_id, arr, room_id);
	let s = serialize_to_vec(&key).expect("failed to serialize");

	assert_eq!(&s, &v, "serialization does not match");

	let key = (user_id, [a, b].into(), room_id);
	let arr: Key<'_> = from_slice(&v).expect("failed to deserialize");

	assert_eq!(arr, key, "deserialization does not match");

	let arr: Key<'_> = from_slice(&s).expect("failed to deserialize");

	assert_eq!(arr, key, "deserialization of serialization does not match");
}

#[test]
fn serde_tuple_option_value_some() {
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let mut aa = Vec::<u8>::new();
	aa.extend_from_slice(room_id.as_bytes());
	aa.push(0xFF);
	aa.extend_from_slice(user_id.as_bytes());

	let bb: (&RoomId, Option<&UserId>) = (room_id, Some(user_id));
	let bbs = serialize_to_vec(&bb).expect("failed to serialize tuple");
	assert_eq!(aa, bbs);

	let cc: (&RoomId, Option<&UserId>) = from_slice(&bbs).expect("failed to deserialize tuple");

	assert_eq!(bb.1, cc.1);
	assert_eq!(cc.0, bb.0);
}

#[test]
fn serde_tuple_option_value_none() {
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();

	let mut aa = Vec::<u8>::new();
	aa.extend_from_slice(room_id.as_bytes());
	aa.push(0xFF);

	let bb: (&RoomId, Option<&UserId>) = (room_id, None);
	let bbs = serialize_to_vec(&bb).expect("failed to serialize tuple");
	assert_eq!(aa, bbs);

	let cc: (&RoomId, Option<&UserId>) = from_slice(&bbs).expect("failed to deserialize tuple");

	assert_eq!(None, cc.1);
	assert_eq!(cc.0, bb.0);
}

#[test]
fn serde_tuple_option_none_value() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let mut aa = Vec::<u8>::new();
	aa.push(0xFF);
	aa.extend_from_slice(user_id.as_bytes());

	let bb: (Option<&RoomId>, &UserId) = (None, user_id);
	let bbs = serialize_to_vec(&bb).expect("failed to serialize tuple");
	assert_eq!(aa, bbs);

	let cc: (Option<&RoomId>, &UserId) = from_slice(&bbs).expect("failed to deserialize tuple");

	assert_eq!(None, cc.0);
	assert_eq!(cc.1, bb.1);
}

#[test]
fn serde_tuple_option_some_value() {
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let mut aa = Vec::<u8>::new();
	aa.extend_from_slice(room_id.as_bytes());
	aa.push(0xFF);
	aa.extend_from_slice(user_id.as_bytes());

	let bb: (Option<&RoomId>, &UserId) = (Some(room_id), user_id);
	let bbs = serialize_to_vec(&bb).expect("failed to serialize tuple");
	assert_eq!(aa, bbs);

	let cc: (Option<&RoomId>, &UserId) = from_slice(&bbs).expect("failed to deserialize tuple");

	assert_eq!(bb.0, cc.0);
	assert_eq!(cc.1, bb.1);
}

#[test]
fn serde_tuple_option_value_incomplete() {
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let mut aa = Vec::<u8>::new();
	aa.extend_from_slice(room_id.as_bytes());
	aa.push(0xFF);
	aa.extend_from_slice(user_id.as_bytes());

	let bb: (&RoomId, &UserId) = (room_id, user_id);
	let bbs = serialize_to_vec(&bb).expect("failed to serialize tuple");
	assert_eq!(aa, bbs);

	let cc: (&RoomId, &UserId, Option<u64>) =
		from_slice(&bbs).expect("failed to deserialize tuple");

	assert_eq!(bb.0, cc.0);
	assert_eq!(bb.1, cc.1);
	assert_eq!(cc.2, None);
}

#[test]
fn serde_tuple_option_some_some() {
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let mut aa = Vec::<u8>::new();
	aa.extend_from_slice(room_id.as_bytes());
	aa.push(0xFF);
	aa.extend_from_slice(user_id.as_bytes());

	let bb: (Option<&RoomId>, Option<&UserId>) = (Some(room_id), Some(user_id));
	let bbs = serialize_to_vec(&bb).expect("failed to serialize tuple");
	assert_eq!(aa, bbs);

	let cc: (Option<&RoomId>, Option<&UserId>) =
		from_slice(&bbs).expect("failed to deserialize tuple");

	assert_eq!(cc.0, bb.0);
	assert_eq!(bb.1, cc.1);
}

#[test]
fn serde_tuple_option_none_none() {
	let aa = vec![0xFF];

	let bb: (Option<&RoomId>, Option<&UserId>) = (None, None);
	let bbs = serialize_to_vec(&bb).expect("failed to serialize tuple");
	assert_eq!(aa, bbs);

	let cc: (Option<&RoomId>, Option<&UserId>) =
		from_slice(&bbs).expect("failed to deserialize tuple");

	assert_eq!(cc.0, bb.0);
	assert_eq!(None, cc.1);
}

#[test]
fn serde_tuple_option_some_none_some() {
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let mut aa = Vec::<u8>::new();
	aa.extend_from_slice(room_id.as_bytes());
	aa.push(0xFF);
	aa.push(0xFF);
	aa.extend_from_slice(user_id.as_bytes());

	let bb: (Option<&RoomId>, Option<&EventId>, Option<&UserId>) =
		(Some(room_id), None, Some(user_id));

	let bbs = serialize_to_vec(&bb).expect("failed to serialize tuple");
	assert_eq!(aa, bbs);

	let cc: (Option<&RoomId>, Option<&EventId>, Option<&UserId>) =
		from_slice(&bbs).expect("failed to deserialize tuple");

	assert_eq!(bb.0, cc.0);
	assert_eq!(None, cc.1);
	assert_eq!(bb.1, cc.1);
	assert_eq!(bb.2, cc.2);
}

#[test]
fn serde_tuple_option_none_none_none() {
	let aa = vec![0xFF, 0xFF];

	let bb: (Option<&RoomId>, Option<&EventId>, Option<&UserId>) = (None, None, None);
	let bbs = serialize_to_vec(&bb).expect("failed to serialize tuple");
	assert_eq!(aa, bbs);

	let cc: (Option<&RoomId>, Option<&EventId>, Option<&UserId>) =
		from_slice(&bbs).expect("failed to deserialize tuple");

	assert_eq!(None, cc.0);
	assert_eq!(bb, cc);
}

#[test]
fn serde_tuple_integer_string() {
	let integer: u64 = 123_456;
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let mut a = integer.to_be_bytes().to_vec();
	a.push(0xFF);
	a.extend_from_slice(user_id.as_bytes());

	let b = (integer, user_id);
	let s = serialize_to_vec(&b).expect("failed to serialize (integer,string) tuple");

	assert_eq!(a, s);

	let c: (u64, &UserId) = from_slice(&s).expect("failed to deserialize (integer,string) tuple");

	assert_eq!(c, b, "deserialized (integer,string) tuple did not match");
}

#[test]
fn serde_tuple_string_integer_string() {
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();
	let integer: u64 = 123_456;
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let mut a = Vec::new();
	a.extend_from_slice(room_id.as_bytes());
	a.push(0xFF);
	a.extend_from_slice(&integer.to_be_bytes());
	a.push(0xFF);
	a.extend_from_slice(user_id.as_bytes());

	let b = (room_id, integer, user_id);
	let s = serialize_to_vec(&b).expect("failed to serialize (string,integer,string) tuple");

	assert_eq!(a, s);

	let c: (&RoomId, u64, &UserId) =
		from_slice(&s).expect("failed to deserialize (integer,string) tuple");

	assert_eq!(c, b, "deserialized (string,integer,string) tuple did not match");
}

#[test]
fn lazy_media_outlives_url_preview() {
	use crate::maps::MAPS;

	let ttl = |name: &str| {
		MAPS.iter()
			.find(|desc| desc.name == name)
			.map(|desc| desc.ttl)
			.expect("descriptor present")
	};

	assert!(
		ttl("mediaid_lazy") >= ttl("url_preview"),
		"a served preview's mxc must still resolve while the preview is cached"
	);
}

#[test]
fn txn_record_golden() {
	let mut batch = WriteBatch::default();
	batch.put(b"key", b"value");
	batch.delete(b"deleted");
	batch.put(b"empty", b"");

	let data = batch.data();
	let mut records = data
		.get(12..)
		.expect("batch shorter than its header");

	assert_eq!(next_record(&mut records), Some((0, b"key".as_slice())));
	assert_eq!(next_record(&mut records), Some((0, b"deleted".as_slice())));
	assert_eq!(next_record(&mut records), Some((0, b"empty".as_slice())));
	assert!(records.is_empty());
}

#[test]
fn txn_record_golden_long_key() {
	let long = [0xAA_u8; 300];

	let mut batch = WriteBatch::default();
	batch.put(long.as_slice(), b"");

	let data = batch.data();
	let mut records = data
		.get(12..)
		.expect("batch shorter than its header");

	assert_eq!(next_record(&mut records), Some((0, long.as_slice())));
	assert!(records.is_empty());
}

#[test]
fn txn_record_cf() {
	// kTypeColumnFamilyValue cf=200 "k"="v", then kTypeColumnFamilyDeletion cf=9
	// "del"
	let mut records: &[u8] =
		&[0x5, 0xC8, 0x1, 0x1, b'k', 0x1, b'v', 0x4, 0x9, 0x3, b'd', b'e', b'l'];

	assert_eq!(next_record(&mut records), Some((200, b"k".as_slice())));
	assert_eq!(next_record(&mut records), Some((9, b"del".as_slice())));
	assert!(records.is_empty());
}

#[test]
fn txn_record_unrecognized() {
	let mut records: &[u8] = &[0x2, 0x1, b'k', 0x1, b'v'];

	assert_eq!(next_record(&mut records), None);
}

#[test]
fn txn_record_truncated() {
	let mut records: &[u8] = &[0x1, 0x5, b'k'];

	assert_eq!(next_record(&mut records), None);
}

#[tokio::test]
async fn txn_insert_raw_preserves_bytes() -> Result {
	let path = format!("/nvme/target/tmp/tuwunel-database-txn-{}", process_id());
	let raw_config = Figment::new()
		.merge(("server_name", "localhost"))
		.merge(("database_path", &path))
		.merge(("test", ["fresh", "cleanup"]));

	let config = Config::new(&raw_config)?;
	let runtime = Handle::current();
	let logging = Logging {
		subscriber: Arc::new(NoSubscriber::new()),
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(State::new()),
	};

	let metrics = Metrics::new(Some(&runtime));
	let server = Arc::new(Server::new(config, Some(&runtime), logging, metrics));
	let database = Database::open(&server).await?;

	let first = database.get("alias_roomid")?;
	let second = database.get("alias_userid")?;
	let first_key: &[u8] = b"\0raw\xFF:key";
	let first_value: &[u8] = b"\xFE\0value";
	let second_key: &[u8] = b"\xFFother\0key";
	let second_value: &[u8] = b"value\0\xFD";

	let mut txn = database.txn();

	txn.insert_raw(first, first_key, first_value);
	txn.insert_raw(second, second_key, second_value);

	let mut keys = txn.keys();
	let (map, key) = keys.next().expect("first queued key");

	assert!(Arc::ptr_eq(&map, first));
	assert_eq!(key, first_key);

	let (map, key) = keys.next().expect("second queued key");

	assert!(Arc::ptr_eq(&map, second));
	assert_eq!(key, second_key);
	assert!(keys.next().is_none());
	drop(keys);

	txn.execute();

	assert_eq!(first.get(&first_key).await?.as_ref(), first_value);
	assert_eq!(second.get(&second_key).await?.as_ref(), second_value);

	drop(database);
	drop(server);

	Ok(())
}
