#![cfg(test)]
#![allow(unused_features)] // 1.96.0-nightly 2026-03-07 bug

use std::{fmt::Debug, hint::black_box};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use minicbor::{
	Encoder, decode as minicbor_decode, encode::write::Cursor, to_vec as minicbor_to_vec,
};
use minicbor_serde::{from_slice as cbor_from_slice, to_vec as cbor_to_vec};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{from_slice as json_from_slice, to_vec as json_to_vec};
use tuwunel_core::ruma::{RoomId, UserId};
use tuwunel_database::{Cbor, Json, deserialize_from_slice, serialize_to_vec};

type SmallTuple = (u64, bool, String);
type CoreTuple<'a> = (u64, bool, &'a str);

#[derive(Clone, Copy)]
struct ScalarCase {
	name: &'static str,
	value: u64,
	bytes: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct Record {
	room_id: String,
	event_id: String,
	count: u64,
	active: bool,
	samples: Vec<u64>,
}

const SCALARS: [ScalarCase; 6] = [
	ScalarCase { name: "zero_1b", value: 0, bytes: 1 },
	ScalarCase { name: "first_2b", value: 24, bytes: 2 },
	ScalarCase { name: "first_3b", value: 256, bytes: 3 },
	ScalarCase {
		name: "first_5b",
		value: 65_536,
		bytes: 5,
	},
	ScalarCase {
		name: "first_9b",
		value: 4_294_967_296,
		bytes: 9,
	},
	ScalarCase {
		name: "max_9b",
		value: u64::MAX,
		bytes: 9,
	},
];

criterion_group!(benches, database_codec, formats, minicbor_core);

criterion_main!(benches);

fn database_codec(b: &mut Criterion) {
	let user_id: &UserId = "@user:example.com"
		.try_into()
		.expect("invalid benchmark user id");

	let room_id: &RoomId = "!room:example.com"
		.try_into()
		.expect("invalid benchmark room id");

	let key = (user_id, room_id);
	let encoded = serialize_to_vec(key).expect("failed to serialize benchmark key");
	let bytes = u64::try_from(encoded.len()).expect("benchmark key length does not fit u64");
	let mut group = b.benchmark_group("database/key_pair");

	group.throughput(Throughput::Bytes(bytes));

	group.bench_function("serialize", |c| {
		c.iter(|| serialize_to_vec(black_box(key)).expect("failed to serialize benchmark key"));
	});

	group.bench_function("deserialize", |c| {
		c.iter(|| {
			deserialize_from_slice::<(&UserId, &RoomId)>(black_box(encoded.as_slice()))
				.expect("failed to deserialize benchmark key")
		});
	});

	group.finish();

	let value = 0x0102_0304_0506_0708_u64;
	let encoded = serialize_to_vec(value).expect("failed to serialize benchmark integer");
	let bytes = u64::try_from(encoded.len()).expect("benchmark integer length does not fit u64");
	let mut group = b.benchmark_group("database/u64");

	group.throughput(Throughput::Bytes(bytes));

	group.bench_function("serialize", |c| {
		c.iter(|| {
			serialize_to_vec(black_box(value)).expect("failed to serialize benchmark integer")
		});
	});

	group.bench_function("deserialize", |c| {
		c.iter(|| {
			deserialize_from_slice::<u64>(black_box(encoded.as_slice()))
				.expect("failed to deserialize benchmark integer")
		});
	});

	group.finish();
}

fn formats(b: &mut Criterion) {
	let scalar = 42_u64;
	let tuple: SmallTuple = (42, true, String::from("matrix"));
	let record = Record {
		room_id: String::from("!room:example.com"),
		event_id: String::from("$event:example.com"),
		count: 65_536,
		active: true,
		samples: Vec::from([0, 23, 24, 255, 256, 65_535, 65_536, u32::MAX.into()]),
	};

	bench_formats(b, "scalar", &scalar);
	bench_formats(b, "tuple", &tuple);
	bench_formats(b, "record", &record);
}

fn bench_formats<T>(b: &mut Criterion, name: &'static str, value: &T)
where
	T: Debug + DeserializeOwned + PartialEq + Serialize,
{
	bench_cbor(b, name, value);
	bench_json(b, name, value);
}

fn bench_cbor<T>(b: &mut Criterion, name: &'static str, value: &T)
where
	T: Debug + DeserializeOwned + PartialEq + Serialize,
{
	let direct = cbor_to_vec(value).expect("failed to directly serialize CBOR benchmark value");
	let database =
		serialize_to_vec(Cbor(value)).expect("failed to database-serialize CBOR benchmark value");

	assert_eq!(database, direct, "database and direct CBOR output differ");

	let decoded: Cbor<T> = deserialize_from_slice(&database)
		.expect("failed to database-deserialize CBOR benchmark value");

	assert_eq!(&decoded.0, value, "database CBOR roundtrip differs");

	let decoded: T =
		cbor_from_slice(&direct).expect("failed to directly deserialize CBOR benchmark value");

	assert_eq!(&decoded, value, "direct CBOR roundtrip differs");

	let bytes = u64::try_from(direct.len()).expect("CBOR benchmark length does not fit u64");
	let mut group = b.benchmark_group(format!("formats/cbor/{name}"));

	group.throughput(Throughput::Bytes(bytes));

	group.bench_function("database_serialize", |c| {
		c.iter(|| {
			serialize_to_vec(Cbor(black_box(value)))
				.expect("failed to database-serialize CBOR benchmark value")
		});
	});

	group.bench_function("database_deserialize", |c| {
		c.iter(|| {
			deserialize_from_slice::<Cbor<T>>(black_box(database.as_slice()))
				.expect("failed to database-deserialize CBOR benchmark value")
		});
	});

	group.bench_function("direct_serialize", |c| {
		c.iter(|| {
			cbor_to_vec(black_box(value))
				.expect("failed to directly serialize CBOR benchmark value")
		});
	});

	group.bench_function("direct_deserialize", |c| {
		c.iter(|| {
			cbor_from_slice::<T>(black_box(direct.as_slice()))
				.expect("failed to directly deserialize CBOR benchmark value")
		});
	});

	group.bench_function("direct_roundtrip", |c| {
		c.iter(|| {
			let encoded = cbor_to_vec(black_box(value))
				.expect("failed to directly serialize CBOR benchmark value");

			cbor_from_slice::<T>(black_box(encoded.as_slice()))
				.expect("failed to directly roundtrip CBOR benchmark value")
		});
	});

	group.finish();
}

fn bench_json<T>(b: &mut Criterion, name: &'static str, value: &T)
where
	T: Debug + DeserializeOwned + PartialEq + Serialize,
{
	let direct = json_to_vec(value).expect("failed to directly serialize JSON benchmark value");
	let database =
		serialize_to_vec(Json(value)).expect("failed to database-serialize JSON benchmark value");

	assert_eq!(database, direct, "database and direct JSON output differ");

	let decoded: Json<T> = deserialize_from_slice(&database)
		.expect("failed to database-deserialize JSON benchmark value");

	assert_eq!(&decoded.0, value, "database JSON roundtrip differs");

	let decoded: T =
		json_from_slice(&direct).expect("failed to directly deserialize JSON benchmark value");

	assert_eq!(&decoded, value, "direct JSON roundtrip differs");

	let bytes = u64::try_from(direct.len()).expect("JSON benchmark length does not fit u64");
	let mut group = b.benchmark_group(format!("formats/json/{name}"));

	group.throughput(Throughput::Bytes(bytes));

	group.bench_function("database_serialize", |c| {
		c.iter(|| {
			serialize_to_vec(Json(black_box(value)))
				.expect("failed to database-serialize JSON benchmark value")
		});
	});

	group.bench_function("database_deserialize", |c| {
		c.iter(|| {
			deserialize_from_slice::<Json<T>>(black_box(database.as_slice()))
				.expect("failed to database-deserialize JSON benchmark value")
		});
	});

	group.bench_function("direct_serialize", |c| {
		c.iter(|| {
			json_to_vec(black_box(value))
				.expect("failed to directly serialize JSON benchmark value")
		});
	});

	group.bench_function("direct_deserialize", |c| {
		c.iter(|| {
			json_from_slice::<T>(black_box(direct.as_slice()))
				.expect("failed to directly deserialize JSON benchmark value")
		});
	});

	group.bench_function("direct_roundtrip", |c| {
		c.iter(|| {
			let encoded = json_to_vec(black_box(value))
				.expect("failed to directly serialize JSON benchmark value");

			json_from_slice::<T>(black_box(encoded.as_slice()))
				.expect("failed to directly roundtrip JSON benchmark value")
		});
	});

	group.finish();
}

fn minicbor_core(b: &mut Criterion) {
	for ScalarCase { name, value, bytes } in SCALARS {
		let encoded =
			minicbor_to_vec(value).expect("failed to serialize minicbor integer benchmark value");
		assert_eq!(encoded.len(), bytes, "minicbor integer encoded length differs");

		let throughput = u64::try_from(bytes).expect("minicbor integer length does not fit u64");
		let mut group = b.benchmark_group(format!("minicbor/u64/{name}"));

		group.throughput(Throughput::Bytes(throughput));

		group.bench_function("stack_serialize", |c| {
			c.iter(|| encode_u64_stack(black_box(value)));
		});

		group.bench_function("vec_serialize", |c| {
			c.iter(|| {
				minicbor_to_vec(black_box(value))
					.expect("failed to serialize minicbor integer benchmark value")
			});
		});

		group.bench_function("deserialize", |c| {
			c.iter(|| {
				minicbor_decode::<u64>(black_box(encoded.as_slice()))
					.expect("failed to deserialize minicbor integer benchmark value")
			});
		});

		group.bench_function("stack_roundtrip", |c| {
			c.iter(|| {
				let (encoded, len) = encode_u64_stack(black_box(value));

				minicbor_decode::<u64>(black_box(&encoded[..len]))
					.expect("failed to roundtrip minicbor integer benchmark value")
			});
		});

		group.bench_function("vec_roundtrip", |c| {
			c.iter(|| {
				let encoded = minicbor_to_vec(black_box(value))
					.expect("failed to serialize minicbor integer benchmark value");

				minicbor_decode::<u64>(black_box(encoded.as_slice()))
					.expect("failed to roundtrip minicbor integer benchmark value")
			});
		});

		group.finish();
	}

	let value: CoreTuple<'_> = (42, true, "matrix");
	let encoded =
		minicbor_to_vec(value).expect("failed to serialize minicbor tuple benchmark value");
	let bytes = u64::try_from(encoded.len()).expect("minicbor tuple length does not fit u64");
	let mut group = b.benchmark_group("minicbor/tuple");

	group.throughput(Throughput::Bytes(bytes));

	group.bench_function("stack_serialize", |c| {
		c.iter(|| encode_tuple_stack(black_box(value)));
	});

	group.bench_function("vec_serialize", |c| {
		c.iter(|| {
			minicbor_to_vec(black_box(value))
				.expect("failed to serialize minicbor tuple benchmark value")
		});
	});

	group.bench_function("deserialize", |c| {
		c.iter(|| {
			minicbor_decode::<CoreTuple<'_>>(black_box(encoded.as_slice()))
				.expect("failed to deserialize minicbor tuple benchmark value")
		});
	});

	group.bench_function("stack_roundtrip", |c| {
		c.iter(|| {
			let (encoded, len) = encode_tuple_stack(black_box(value));

			let decoded = minicbor_decode::<CoreTuple<'_>>(black_box(&encoded[..len]))
				.expect("failed to roundtrip minicbor tuple benchmark value");

			black_box(decoded);
		});
	});

	group.bench_function("vec_roundtrip", |c| {
		c.iter(|| {
			let encoded = minicbor_to_vec(black_box(value))
				.expect("failed to serialize minicbor tuple benchmark value");

			let decoded = minicbor_decode::<CoreTuple<'_>>(black_box(encoded.as_slice()))
				.expect("failed to roundtrip minicbor tuple benchmark value");

			black_box(decoded);
		});
	});

	group.finish();
}

#[inline]
fn encode_u64_stack(value: u64) -> ([u8; 9], usize) {
	let cursor = Cursor::new([0_u8; 9]);
	let mut encoder = Encoder::new(cursor);
	encoder
		.u64(value)
		.expect("failed to serialize minicbor integer benchmark value");

	let cursor = encoder.into_writer();
	let len = cursor.position();

	(cursor.into_inner(), len)
}

#[inline]
fn encode_tuple_stack(value: CoreTuple<'_>) -> ([u8; 11], usize) {
	let cursor = Cursor::new([0_u8; 11]);
	let mut encoder = Encoder::new(cursor);
	encoder
		.encode(value)
		.expect("failed to serialize minicbor tuple benchmark value");

	let cursor = encoder.into_writer();
	let len = cursor.position();

	(cursor.into_inner(), len)
}
