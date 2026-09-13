//! Persisted CBOR layout pinned against the minicbor-serde 0.6.2 writer.
//!
//! The byte strings were emitted by minicbor-serde 0.6.2 over minicbor 2.2.2,
//! which 0.6.2 over 2.3.0 reproduces exactly. The `Cbor` wrapper stores no
//! version marker, so a dependency bump that changes the mapping of any shape
//! fails here before it reaches a database.
//!
//! To regenerate, build a standalone package pinning those two crates with
//! default features, serialize each value below through `minicbor_serde::to_vec`
//! (the same default `Serializer` over a `Writer` that `ser.rs` delegates to),
//! and hex-encode the output; the second baseline was confirmed identical with
//! `cmp` over the two archives.

use std::{
	collections::BTreeMap,
	fmt::Debug,
	net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
	str::from_utf8,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{Cbor, de::from_slice, ser::serialize_to_vec};

type Nested = BTreeMap<String, Vec<Option<i64>>>;

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct Scalars {
	unsigned: [u64; 12],
	signed: [i64; 12],
	narrow: (u8, u16, u32, i8, i16, i32),
	floats: (f32, f64),
	flags: (bool, bool),
	unit: (),
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct Record {
	#[serde(rename = "display-name")]
	name: String,
	#[serde(default)]
	count: u64,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	optional: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct Bytes {
	#[serde(with = "serde_bytes")]
	bytes: Vec<u8>,
	integers: Vec<u8>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
enum Choice {
	Empty,
	Newtype(u64),
	Tuple(bool, i64),
	Struct {
		label: String,
		count: u64,
	},
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(transparent)]
struct Transparent(String);

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct Clock {
	time: SystemTime,
	duration: Duration,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct Network {
	ipv4: IpAddr,
	ipv6: IpAddr,
	socket: SocketAddr,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct Extensions {
	enabled: bool,
	#[serde(flatten)]
	custom: BTreeMap<String, Value>,
}

#[test]
fn scalars() {
	let scalars = Scalars {
		unsigned: [0, 23, 24, 255, 256, 65535, 65536, u32::MAX.into(), 1 << 32, u64::MAX, 1, 22],
		signed: [
			0,
			-1,
			-24,
			-25,
			-256,
			-257,
			-65536,
			-65537,
			-(1 << 32),
			-(1 << 32) - 1,
			i64::MIN,
			i64::MAX,
		],
		narrow: (u8::MAX, u16::MAX, u32::MAX, i8::MIN, i16::MIN, i32::MIN),
		floats: (1.5, -123.125),
		flags: (false, true),
		unit: (),
	};

	assert_pinned(
		"a668756e7369676e65648c0017181818ff19010019ffff1a000100001affffffff1b0000000100000000\
		 1bffffffffffffffff0116667369676e65648c002037381838ff39010039ffff3a000100003affffffff\
		 3b00000001000000003b7fffffffffffffff1b7fffffffffffffff666e6172726f778618ff19ffff1aff\
		 ffffff387f397fff3a7fffffff66666c6f61747382fa3fc00000fbc05ec8000000000065666c61677382\
		 f4f564756e697480",
		&scalars,
	);
}

#[test]
fn record() {
	let record = Record {
		name: "café 漢字".into(),
		count: 256,
		optional: Some("present".into()),
	};

	assert_pinned(
		"a36c646973706c61792d6e616d656c636166c3a920e6bca2e5ad9765636f756e74190100686f7074696f\
		 6e616c6770726573656e74",
		&record,
	);
}

#[test]
fn record_defaults_and_unknown_keys() {
	let absent = Record {
		name: "minimal".into(),
		count: 0,
		optional: None,
	};

	assert_pinned("a26c646973706c61792d6e616d65676d696e696d616c65636f756e7400", &absent);
	for legacy in [
		"a16c646973706c61792d6e616d65676d696e696d616c",
		"a26c646973706c61792d6e616d65676d696e696d616c67756e6b6e6f776e6769676e6f726564",
	] {
		let Cbor(decoded): Cbor<Record> = from_slice(&hex(legacy)).expect("schema drift read");

		assert_eq!(decoded, absent);
	}
}

#[test]
fn bytes_versus_integer_arrays() {
	let bytes = Bytes {
		bytes: vec![0, 23, 24, 255],
		integers: vec![0, 23, 24, 255],
	};

	assert_pinned("a265627974657344001718ff68696e746567657273840017181818ff", &bytes);
}

#[test]
fn enum_forms() {
	let named = Choice::Struct { label: "named".into(), count: 24 };
	let choices = [Choice::Empty, Choice::Newtype(256), Choice::Tuple(true, -25), named];

	assert_pinned(
		"8465456d707479a1674e657774797065190100a1655475706c6582f53818a166537472756374a2656c61\
		 62656c656e616d656465636f756e741818",
		&choices,
	);
}

#[test]
fn nested_and_transparent() {
	let nested: Nested =
		[("alpha".into(), vec![Some(-1), None, Some(256)]), ("empty".into(), Vec::new())].into();

	assert_pinned("a265616c7068618320f619010065656d70747980", &nested);
	assert_pinned("666f7061717565", &Transparent("opaque".into()));
}

#[test]
fn clock() {
	let time = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
	let clock = Clock {
		time,
		duration: Duration::new(24, 999_999_999),
	};

	assert_pinned(
		"a26474696d65a270736563735f73696e63655f65706f63681a6553f100716e616e6f735f73696e63655f\
		 65706f63681a075bcd15686475726174696f6ea264736563731818656e616e6f731a3b9ac9ff",
		&clock,
	);
}

#[test]
fn network() {
	let network = Network {
		ipv4: Ipv4Addr::new(192, 0, 2, 1).into(),
		ipv6: Ipv6Addr::LOCALHOST.into(),
		socket: SocketAddr::from(([192, 0, 2, 2], 8448)),
	};

	assert_pinned(
		"a36469707634a16256348418c00002016469707636a1625636900000000000000000000000000000000166\
		 736f636b6574a1625634828418c0000202192100",
		&network,
	);
}

#[test]
fn flattened_extensions() {
	let custom = json!({"ratio": 1.5, "values": [true, false, -24, "text"]});
	let extensions = Extensions {
		enabled: true,
		custom: [("custom".into(), custom)].into(),
	};

	assert_pinned(
		"bf67656e61626c6564f566637573746f6da265726174696ffb3ff80000000000006676616c75657384f5f4\
		 376474657874ff",
		&extensions,
	);
}

// A dynamic JSON null has always been written as an empty array and read back
// as one; this pins the loss rather than the value.
#[test]
fn json_null_is_an_empty_array() {
	let Cbor(null): Cbor<Value> = from_slice(&[0x80]).expect("legacy read");

	assert_eq!(null, Value::Array(Vec::new()));
	assert_eq!(serialize_to_vec(Cbor(Value::Null)).expect("write"), [0x80]);
}

#[test]
fn malformed_and_truncated_values_fail() {
	for bytes in [&[][..], &[0xFF], &[0x18], &[0x1A, 0, 0], &[0x9F, 1]] {
		from_slice::<Cbor<u64>>(bytes).expect_err("malformed integer");
	}

	for bytes in [&[0x63, b'a'][..], &[0x61, 0xFF]] {
		from_slice::<Cbor<String>>(bytes).expect_err("malformed string");
	}

	from_slice::<Cbor<Vec<u64>>>(&[0x82, 1]).expect_err("truncated sequence");
	from_slice::<Cbor<BTreeMap<String, u64>>>(&[0xA1, 0x61, b'k'])
		.expect_err("map missing its value");
}

#[test]
fn trailing_bytes_are_not_validated() {
	let Cbor(value): Cbor<u64> = from_slice(&[0x01, 0x02]).expect("leading value");

	assert_eq!(value, 1);
}

fn assert_pinned<T>(legacy: &str, expected: &T)
where
	T: Debug + DeserializeOwned + PartialEq + Serialize,
{
	let legacy = hex(legacy);
	let Cbor(decoded): Cbor<T> = from_slice(&legacy).expect("legacy read");

	assert_eq!(&decoded, expected);
	assert_eq!(serialize_to_vec(Cbor(expected)).expect("write"), legacy);
}

fn hex(text: &str) -> Vec<u8> {
	text.as_bytes()
		.chunks(2)
		.map(|pair| from_utf8(pair).expect("hex fixture"))
		.map(|pair| u8::from_str_radix(pair, 16).expect("hex fixture"))
		.collect()
}
