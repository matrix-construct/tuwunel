//! Derives LiveKit room names and participant identities (MSC4195).
//!
//! Both are the unpadded standard base64 of a SHA-256 over the canonical JSON
//! of a two-string array. That is MSC4195's example derivation, so a call
//! keeps one LiveKit room whichever service issued its tokens.

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use ruma::{RoomId, UserId};
use tuwunel_core::{
	arrayvec::ArrayString, smallvec::SmallVec, utils::hash::sha256::hash as sha256,
};

// An unpadded base64 SHA-256 digest.
pub(super) type Name = ArrayString<NAME_LEN>;

// The canonical JSON of two IDs, inline at typical ID lengths.
type Json = SmallVec<[u8; 128]>;

const NAME_LEN: usize = 43;

// The call slot as Element Call and matrix-js-sdk send it, then as MSC4196 spells it.
const UNSTABLE_CALL_SLOT: &str = "m.call#ROOM";
const CALL_SLOT: &str = "m.call#room";

/// Names the LiveKit room of a call slot.
///
/// Every slot hashes to its own room except the two spellings of the call
/// slot, which share one, so clients sending either meet in the same call.
pub(super) fn room_name(room_id: &RoomId, slot_id: &str) -> Name {
	// MSC4196 allows `m.call` one slot, `room`; its reference implementation spells it `ROOM`.
	let slot_id = if slot_id == UNSTABLE_CALL_SLOT {
		CALL_SLOT
	} else {
		slot_id
	};

	derive(room_id.as_str(), slot_id)
}

/// Names a call member's LiveKit participant.
///
/// The user ID is part of the hash, so a user can only claim the member IDs
/// of their own call memberships.
pub(super) fn identity(user_id: &UserId, member_id: &str) -> Name {
	derive(user_id.as_str(), member_id)
}

pub(super) fn derive(first: &str, second: &str) -> Name {
	encode(&sha256(canonical(first, second)))
}

fn canonical(first: &str, second: &str) -> Json {
	let mut json = Json::new();

	// serde_json's compact output for an array of strings is canonical JSON.
	serde_json::to_writer(&mut json, &[first, second]).expect("two strings always serialize");
	json
}

fn encode(digest: &[u8]) -> Name {
	let mut encoded = [0_u8; NAME_LEN];

	STANDARD_NO_PAD
		.encode_slice(digest, &mut encoded)
		.expect("32 bytes always encode to 43 base64 characters");

	Name::from_byte_string(&encoded).expect("base64 output is always ASCII")
}
