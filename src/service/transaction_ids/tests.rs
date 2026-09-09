#![cfg(test)]

use ruma::{DeviceId, RoomId, TransactionId, UserId};
use tuwunel_database::serialize_to_vec;

use super::{room_txnid_key, txnid_key};

const USER: &str = "@user:example.com";
const DEVICE: &str = "DEVICE";
const TXN: &str = "transaction";
const ROOM: &str = "!room:example.com";
const OTHER_ROOM: &str = "!other:example.com";

#[test]
fn legacy_key_bytes_match_the_original_tuple_codec() {
	let actual =
		serialize_to_vec(txnid_key(user(), Some(device()), txn())).expect("serialize legacy key");

	assert_eq!(actual.as_slice(), b"@user:example.com\xFFDEVICE\xFFtransaction");
}

#[test]
fn device_less_legacy_key_bytes_remain_unchanged() {
	let actual =
		serialize_to_vec(txnid_key(user(), None, txn())).expect("serialize device-less key");

	assert_eq!(actual.as_slice(), b"@user:example.com\xFF\xFFtransaction");
}

#[test]
fn room_key_appends_tag_room_and_event_type() {
	let key = room_txnid_key(user(), Some(device()), txn(), room(), "m.room.message");
	let actual = serialize_to_vec(key).expect("serialize room key");

	assert_eq!(
		actual.as_slice(),
		b"@user:example.com\xFFDEVICE\xFFtransaction\xFFroom-send\xFF!room:example.com\xFFm.room.message",
	);
}

#[test]
fn device_less_room_key_preserves_empty_device_field() {
	let key = room_txnid_key(user(), None, txn(), room(), "m.room.message");
	let actual = serialize_to_vec(key).expect("serialize device-less room key");

	assert_eq!(
		actual.as_slice(),
		b"@user:example.com\xFF\xFFtransaction\xFFroom-send\xFF!room:example.com\xFFm.room.message",
	);
}

#[test]
fn room_and_event_type_are_independent_key_scopes() {
	let message = room_txnid_key(user(), Some(device()), txn(), room(), "m.room.message");
	let encrypted = room_txnid_key(user(), Some(device()), txn(), room(), "m.room.encrypted");
	let other = room_txnid_key(user(), Some(device()), txn(), other_room(), "m.room.message");
	let message = serialize_to_vec(message).expect("serialize message key");
	let encrypted = serialize_to_vec(encrypted).expect("serialize encrypted key");
	let other = serialize_to_vec(other).expect("serialize other room key");
	let legacy =
		serialize_to_vec(txnid_key(user(), Some(device()), txn())).expect("serialize legacy key");

	assert_ne!(message, encrypted);
	assert_ne!(message, other);
	assert_ne!(message, legacy);
}

fn user() -> &'static UserId { USER.try_into().expect("valid user ID") }

fn device() -> &'static DeviceId { DEVICE.into() }

fn txn() -> &'static TransactionId { TXN.into() }

fn room() -> &'static RoomId { ROOM.try_into().expect("valid room ID") }

fn other_room() -> &'static RoomId { OTHER_ROOM.try_into().expect("valid room ID") }
