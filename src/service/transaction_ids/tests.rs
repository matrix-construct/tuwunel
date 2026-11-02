#![cfg(test)]

//! Tests for transaction-key compatibility and room scoping.
//!
//! The cases preserve the original legacy key bytes. They also prove that the
//! room tag, room identifier, and scope (an event type for a send, a target for
//! a redaction) independently separate room keys.

use ruma::{DeviceId, EventId, RoomId, TransactionId, UserId};
use tuwunel_database::serialize_to_vec;

use super::{RoomScope, RoomTxn, room_txnid_key, txnid_key};

const USER: &str = "@user:example.com";
const DEVICE: &str = "DEVICE";
const TXN: &str = "transaction";
const ROOM: &str = "!room:example.com";
const OTHER_ROOM: &str = "!other:example.com";
const EVENT: &str = "$event:example.com";
const OTHER_EVENT: &str = "$other:example.com";

#[test]
fn legacy_key_bytes_match_the_original_tuple_codec() {
	assert_eq!(legacy_key(Some(device())), b"@user:example.com\xFFDEVICE\xFFtransaction");
}

#[test]
fn device_less_legacy_key_bytes_remain_unchanged() {
	assert_eq!(legacy_key(None), b"@user:example.com\xFF\xFFtransaction");
}

#[test]
fn room_key_appends_tag_room_and_event_type() {
	assert_eq!(
		room_key(Some(device()), room(), RoomScope::Send("m.room.message")),
		b"@user:example.com\xFFDEVICE\xFFtransaction\xFFroom-send\xFF!room:example.com\xFFm.room.message",
	);
}

#[test]
fn device_less_room_key_preserves_empty_device_field() {
	assert_eq!(
		room_key(None, room(), RoomScope::Send("m.room.message")),
		b"@user:example.com\xFF\xFFtransaction\xFFroom-send\xFF!room:example.com\xFFm.room.message",
	);
}

#[test]
fn redact_key_appends_tag_room_and_target() {
	assert_eq!(
		room_key(Some(device()), room(), RoomScope::Redact(event())),
		b"@user:example.com\xFFDEVICE\xFFtransaction\xFFroom-redact\xFF!room:example.com\xFF$event:example.com",
	);
}

#[test]
fn room_and_event_type_are_independent_key_scopes() {
	let message = room_key(Some(device()), room(), RoomScope::Send("m.room.message"));

	assert_ne!(message, room_key(Some(device()), room(), RoomScope::Send("m.room.encrypted")));
	assert_ne!(
		message,
		room_key(Some(device()), other_room(), RoomScope::Send("m.room.message"))
	);

	assert_ne!(message, legacy_key(Some(device())));
}

#[test]
fn redact_tag_and_target_are_independent_key_scopes() {
	let redaction = room_key(Some(device()), room(), RoomScope::Redact(event()));

	assert_ne!(redaction, room_key(Some(device()), room(), RoomScope::Redact(other_event())));
	assert_ne!(redaction, room_key(Some(device()), room(), RoomScope::Send(EVENT)));
}

fn legacy_key(device_id: Option<&DeviceId>) -> Vec<u8> {
	serialize_to_vec(txnid_key(user(), device_id, txn())).expect("serialize legacy key")
}

fn room_key(device_id: Option<&DeviceId>, room_id: &RoomId, scope: RoomScope<'_>) -> Vec<u8> {
	let room_txn = RoomTxn {
		user_id: user(),
		device_id,
		txn_id: txn(),
		room_id,
	};

	serialize_to_vec(room_txnid_key(room_txn, scope)).expect("serialize room key")
}

fn user() -> &'static UserId { USER.try_into().expect("valid user ID") }

fn device() -> &'static DeviceId { DEVICE.into() }

fn txn() -> &'static TransactionId { TXN.into() }

fn room() -> &'static RoomId { ROOM.try_into().expect("valid room ID") }

fn event() -> &'static EventId { EVENT.try_into().expect("valid event ID") }

fn other_room() -> &'static RoomId { OTHER_ROOM.try_into().expect("valid room ID") }

fn other_event() -> &'static EventId { OTHER_EVENT.try_into().expect("valid event ID") }
