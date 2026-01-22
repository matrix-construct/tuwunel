#![expect(clippy::needless_borrows_for_generic_args)]

use std::fmt::Debug;

use serde::Serialize;
use tuwunel_core::{
	arrayvec::ArrayVec,
	ruma::{EventId, RoomId, UserId, serde::Raw},
};

use crate::{
	Cbor, Ignore, Interfix, de, ser,
	ser::{Json, serialize_to_vec},
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
	let deserialized: FilterDefinition = de::from_slice::<Cbor<_>>(&serialized)
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
	let deserialized: Foo = de::from_slice::<Cbor<_>>(&serialized)
		.expect("failed to deserialize cbor")
		.0;

	assert_eq!(foo.a, deserialized.a);
	assert_eq!(foo.a.get(), deserialized.b.get());
}

#[test]
fn de_tuple() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com\xFF!room:example.com";
	let (a, b): (&UserId, &RoomId) = de::from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, room_id, "deserialized room_id does not match");
}

#[test]
#[should_panic(expected = "failed to deserialize")]
fn de_tuple_invalid() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com\xFF@user:example.com";
	let (a, b): (&UserId, &RoomId) = de::from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, room_id, "deserialized room_id does not match");
}

#[test]
#[should_panic(expected = "failed to deserialize")]
fn de_tuple_incomplete() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com";
	let (a, _): (&UserId, &RoomId) = de::from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
}

#[test]
fn de_tuple_incomplete_default() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com";
	let (a, b): (&UserId, &str) = de::from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, "", "deserialized defaulted str does not match");
}

#[test]
#[should_panic(expected = "failed to deserialize")]
fn de_tuple_incomplete_nodefault() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com";
	let (a, _): (&UserId, u64) = de::from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
}

#[test]
fn de_tuple_incomplete_option() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com";
	let (a, b): (&UserId, Option<&str>) = de::from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, None, "deserialized defaulted Option does not match");
}

#[test]
#[should_panic(expected = "failed to deserialize")]
fn de_tuple_incomplete_with_sep() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com\xFF";
	let (a, _): (&UserId, &RoomId) = de::from_slice(raw).expect("failed to deserialize");

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
	let (a, b): (&UserId, &RoomId) = de::from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(b, room_id, "deserialized room_id does not match");
}

#[test]
fn de_tuple_ignore() {
	let user_id: &UserId = "@user:example.com".try_into().unwrap();
	let room_id: &RoomId = "!room:example.com".try_into().unwrap();

	let raw: &[u8] = b"@user:example.com\xFF@user2:example.net\xFF!room:example.com";
	let (a, _, c): (&UserId, Ignore, &RoomId) =
		de::from_slice(raw).expect("failed to deserialize");

	assert_eq!(a, user_id, "deserialized user_id does not match");
	assert_eq!(c, room_id, "deserialized room_id does not match");
}

#[test]
fn de_json_array() {
	let a = &["foo", "bar", "baz"];
	let s = serde_json::to_vec(a).expect("failed to serialize to JSON array");

	let b: Raw<Vec<Raw<String>>> = de::from_slice(&s).expect("failed to deserialize");

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

	let b: Raw<Vec<Raw<String>>> = de::from_slice(&s).expect("failed to deserialize");

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

	let arv: ArrayVec<u64, 2> = de::from_slice::<ArrayVec<u64, 2>>(v.as_slice())
		.map(TryInto::try_into)
		.expect("failed to deserialize to arrayvec")
		.expect("failed to deserialize into");

	assert_eq!(arv[0], a, "deserialized arv [0] does not match");
	assert_eq!(arv[1], b, "deserialized arv [1] does not match");

	let arr: [u64; 2] = de::from_slice::<[u64; 2]>(v.as_slice())
		.map(TryInto::try_into)
		.expect("failed to deserialize to array")
		.expect("failed to deserialize into");

	assert_eq!(arr[0], a, "deserialized arr [0] does not match");
	assert_eq!(arr[1], b, "deserialized arr [1] does not match");

	let vec: Vec<u64> = de::from_slice(v.as_slice()).expect("failed to deserialize to vec");

	assert_eq!(vec[0], a, "deserialized vec [0] does not match");
	assert_eq!(vec[1], b, "deserialized vec [1] does not match");
}

#[test]
fn de_array_string() {
	let a = "foo";
	let b = "bar";
	let v = b"foo\xFFbar";

	let arv: ArrayVec<&str, 2> = de::from_slice::<ArrayVec<&str, 2>>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to arrayvec")
		.expect("failed to deserialize into");
	assert_eq!(arv[0], a, "deserialized arv [0] does not match");
	assert_eq!(arv[1], b, "deserialized arv [1] does not match");
	assert_eq!(arv.len(), 2);

	let arv: ArrayVec<String, 2> = de::from_slice::<ArrayVec<String, 2>>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to arrayvec")
		.expect("failed to deserialize into");
	assert_eq!(arv[0], a, "deserialized arv [0] does not match");
	assert_eq!(arv[1], b, "deserialized arv [1] does not match");
	assert_eq!(arv.len(), 2);

	let arr: [&str; 2] = de::from_slice::<[&str; 2]>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to array")
		.expect("failed to deserialize into");
	assert_eq!(arr[0], a, "deserialized arr [0] does not match");
	assert_eq!(arr[1], b, "deserialized arr [1] does not match");

	let arr: [String; 2] = de::from_slice::<[String; 2]>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to array")
		.expect("failed to deserialize into");
	assert_eq!(arr[0], a, "deserialized arr [0] does not match");
	assert_eq!(arr[1], b, "deserialized arr [1] does not match");

	let vec: Vec<&str> = de::from_slice(v).expect("failed to deserialize to vec");
	assert_eq!(vec[0], a, "deserialized vec [0] does not match");
	assert_eq!(vec[1], b, "deserialized vec [1] does not match");
	assert_eq!(vec.len(), 2);

	let vec: Vec<String> = de::from_slice(v).expect("failed to deserialize to vec");
	assert_eq!(vec[0], a, "deserialized vec [0] does not match");
	assert_eq!(vec[1], b, "deserialized vec [1] does not match");
	assert_eq!(vec.len(), 2);
}

#[test]
fn de_array_one_string() {
	let a = "foo";
	let v = b"foo";

	let arv: ArrayVec<&str, 1> = de::from_slice::<ArrayVec<&str, 1>>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to arrayvec")
		.expect("failed to deserialize into");
	assert_eq!(arv[0], a, "deserialized arv [0] does not match");
	assert_eq!(arv.len(), 1);

	let arv: ArrayVec<String, 1> = de::from_slice::<ArrayVec<String, 1>>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to arrayvec")
		.expect("failed to deserialize into");
	assert_eq!(arv[0], a, "deserialized arv [0] does not match");
	assert_eq!(arv.len(), 1);

	let arr: [&str; 1] = de::from_slice::<[&str; 1]>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to array")
		.expect("failed to deserialize into");
	assert_eq!(arr[0], a, "deserialized arr [0] does not match");

	let arr: [String; 1] = de::from_slice::<[String; 1]>(v)
		.map(TryInto::try_into)
		.expect("failed to deserialize to array")
		.expect("failed to deserialize into");
	assert_eq!(arr[0], a, "deserialized arr [0] does not match");

	let vec: Vec<&str> = de::from_slice(v).expect("failed to deserialize to vec");
	assert_eq!(vec[0], a, "deserialized vec [0] does not match");
	assert_eq!(vec.len(), 1);

	let vec: Vec<String> = de::from_slice(v).expect("failed to deserialize to vec");
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
	let arr: Key<'_> = de::from_slice(&v).expect("failed to deserialize");

	assert_eq!(arr, key, "deserialization does not match");

	let arr: Key<'_> = de::from_slice(&s).expect("failed to deserialize");

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

	let cc: (&RoomId, Option<&UserId>) =
		de::from_slice(&bbs).expect("failed to deserialize tuple");

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

	let cc: (&RoomId, Option<&UserId>) =
		de::from_slice(&bbs).expect("failed to deserialize tuple");

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

	let cc: (Option<&RoomId>, &UserId) =
		de::from_slice(&bbs).expect("failed to deserialize tuple");

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

	let cc: (Option<&RoomId>, &UserId) =
		de::from_slice(&bbs).expect("failed to deserialize tuple");

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
		de::from_slice(&bbs).expect("failed to deserialize tuple");

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
		de::from_slice(&bbs).expect("failed to deserialize tuple");

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
		de::from_slice(&bbs).expect("failed to deserialize tuple");

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
		de::from_slice(&bbs).expect("failed to deserialize tuple");

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
		de::from_slice(&bbs).expect("failed to deserialize tuple");

	assert_eq!(None, cc.0);
	assert_eq!(bb, cc);
}
