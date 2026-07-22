//! Binary codec and fast hashing for event IDs.
//!
//! Since room v3 an event ID is the event's sha256 reference hash: `$` followed
//! by 43 characters of unpadded base64 (standard alphabet in v3, URL-safe in
//! v4+), so the 32 hash bytes and the ID convert losslessly in both directions.
//!
//! TODO: Move this into Ruma and generalize for other identifiers.

use std::{
	hash::{BuildHasher, Hasher},
	ops::BitOr,
	sync::LazyLock,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::random;
use ruma::{EventId, OwnedEventId};

/// The sha256 reference hash encoded by a v3+ event ID.
pub use crate::utils::hash::sha256::Digest as Sha256;

/// `BuildHasher` for event-ID-keyed maps and sets.
///
/// Uniformly distributed bits are recovered from the ID's base64 payload
/// instead of running a byte hasher over the 44-byte string; other keys fall
/// back to a folded-multiply hash. Seeded once per process; prefer `SipHash`
/// where an attacker who can grind key bits must not learn bucket placement.
#[derive(Clone, Copy, Debug, Default)]
pub struct RandomState;

/// Streaming state for [`RandomState`].
#[derive(Clone, Copy, Debug, Default)]
pub struct FoldHasher(u64);

// The odd seed keeps the folded multiply bijective.
static SEED: LazyLock<u64> = LazyLock::new(|| random::<u64>() | 1);

static SEXTETS: [u8; SEXTETS_LEN] = sextets();

const ALPHABET_STANDARD: &[u8; ALPHABET_LEN] =
	b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

const ALPHABET_URL_SAFE: &[u8; ALPHABET_LEN] =
	b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

const SEXTETS_LEN: usize = 256;
const ALPHABET_LEN: usize = 64;
const ENCODED_LEN: usize = 43;

const INVALID: u8 = 0xFF;

/// Decode an event ID into the sha256 it encodes. `None` unless the ID has the
/// v3+ shape with canonical unpadded base64 in either alphabet.
#[must_use]
pub fn decode(event_id: &EventId) -> Option<Sha256> { decode_bytes(event_id.as_bytes()) }

/// Encode a sha256 into the URL-safe (room v4+) form of the event ID. A v3
/// (standard-alphabet) ID does not round-trip through this form.
#[must_use]
pub fn encode(sha256: &Sha256) -> OwnedEventId {
	let mut encoded = [0_u8; ENCODED_LEN];
	URL_SAFE_NO_PAD
		.encode_slice(sha256, &mut encoded)
		.expect("32 bytes always encode to 43 base64 characters");

	let encoded: &str = str::from_utf8(&encoded).expect("base64 output is always ASCII");

	OwnedEventId::from_parts('$', encoded, None).expect("valid event ID from base64 encoding")
}

impl BuildHasher for RandomState {
	type Hasher = FoldHasher;

	#[inline]
	fn build_hasher(&self) -> FoldHasher { FoldHasher::default() }
}

impl Hasher for FoldHasher {
	#[inline]
	fn finish(&self) -> u64 { self.0 }

	#[inline]
	fn write(&mut self, bytes: &[u8]) { self.0 = fold(self.0 ^ hash_bytes(bytes)); }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
	decode_bytes(bytes).map_or_else(
		|| fold_bytes(bytes),
		|sha256| {
			sha256
				.first_chunk()
				.map(|first| u64::from_be_bytes(*first))
				.expect("sha256 wider than u64")
		},
	)
}

fn fold_bytes(bytes: &[u8]) -> u64 {
	let head = u64::try_from(bytes.len()).unwrap_or(u64::MAX);

	bytes.chunks(8).fold(head, |acc, chunk| {
		let mut word = [0_u8; 8];

		word[..chunk.len()].copy_from_slice(chunk);
		fold(acc ^ u64::from_be_bytes(word))
	})
}

#[inline]
fn fold(word: u64) -> u64 { folded_multiply(word, *SEED) }

#[inline]
#[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
fn folded_multiply(a: u64, b: u64) -> u64 {
	let wide = u128::from(a).wrapping_mul(u128::from(b));

	(wide as u64) ^ ((wide >> 64) as u64)
}

fn decode_bytes(bytes: &[u8]) -> Option<Sha256> {
	bytes
		.strip_prefix(b"$")
		.and_then(|encoded| encoded.try_into().ok())
		.and_then(decode_encoded)
}

fn decode_encoded(encoded: &[u8; ENCODED_LEN]) -> Option<Sha256> {
	let sextet = |byte: &u8| u32::from(SEXTETS[usize::from(*byte)]);
	let pack = |bytes: &[u8]| {
		bytes
			.iter()
			.map(sextet)
			.fold(0_u32, |group, sextet| (group << 6) | sextet)
	};

	let invalid = encoded
		.iter()
		.map(sextet)
		.fold(0_u32, BitOr::bitor);

	let mut sha256 = Sha256::default();
	let (quads, tail) = encoded.as_chunks::<4>();
	let (triples, _) = sha256.as_chunks_mut::<3>();

	for (quad, bytes) in quads.iter().zip(triples) {
		let [_, b0, b1, b2] = pack(quad).to_be_bytes();
		*bytes = [b0, b1, b2];
	}

	// The tail's three sextets carry the last two bytes; unpadded canonical
	// base64 requires the two excess low bits to be zero.
	let group = pack(tail);
	let [_, _, b30, b31] = (group >> 2).to_be_bytes();

	sha256[30] = b30;
	sha256[31] = b31;
	(invalid <= 0x3F && group.trailing_zeros() >= 2).then_some(sha256)
}

#[expect(clippy::as_conversions)]
const fn sextets() -> [u8; SEXTETS_LEN] {
	let mut table = [INVALID; SEXTETS_LEN];
	let mut value = 0_u8;
	while value < 64 {
		table[ALPHABET_STANDARD[value as usize] as usize] = value;
		table[ALPHABET_URL_SAFE[value as usize] as usize] = value;
		value = value.wrapping_add(1);
	}

	table
}

#[cfg(test)]
mod tests {
	use std::{
		collections::{HashMap, HashSet},
		iter::repeat_with,
	};

	use base64::engine::general_purpose::STANDARD_NO_PAD;

	use super::*;
	use crate::utils::rand::event_id as random_event_id;

	#[test]
	fn roundtrip_random() {
		for _ in 0..64 {
			let event_id = random_event_id();
			let sha256 = decode(&event_id).expect("random v4 event ID decodes");

			assert_eq!(encode(&sha256), event_id);
		}
	}

	#[test]
	fn decode_standard_alphabet() {
		for _ in 0..64 {
			let sha256 = decode(&random_event_id()).unwrap();

			let mut encoded = String::from("$");
			STANDARD_NO_PAD.encode_string(sha256, &mut encoded);

			let standard: OwnedEventId = encoded.try_into().unwrap();

			assert_eq!(decode(&standard), Some(sha256));
		}
	}

	#[test]
	fn decode_rejects_non_v3_shapes() {
		let legacy: OwnedEventId = "$legacy_event:server.example".try_into().unwrap();

		assert_eq!(decode(&legacy), None);

		let event_id = random_event_id();
		let short: OwnedEventId = event_id
			.as_str()
			.get(..40)
			.unwrap()
			.try_into()
			.unwrap();

		assert_eq!(decode(&short), None);
	}

	#[test]
	fn decode_rejects_non_canonical_tail() {
		let event_id = random_event_id();
		let mut noncanonical = event_id.as_str().to_owned();

		// 'B' decodes to sextet 1, leaving a nonzero excess bit in the tail.
		noncanonical.replace_range(43.., "B");
		let noncanonical: OwnedEventId = noncanonical.try_into().unwrap();

		assert_eq!(decode(&noncanonical), None);
	}

	#[test]
	fn map_operations() {
		let mut map: HashMap<OwnedEventId, usize, RandomState> = HashMap::default();
		let mut set: HashSet<OwnedEventId, RandomState> = HashSet::default();

		let ids: Vec<OwnedEventId> = repeat_with(random_event_id)
			.take(256)
			.chain(["$legacy_event:server.example".try_into().unwrap()])
			.collect();

		for (i, event_id) in ids.iter().enumerate() {
			map.insert(event_id.clone(), i);
			set.insert(event_id.clone());
		}

		for (i, event_id) in ids.iter().enumerate() {
			let event_id: &EventId = event_id;

			assert_eq!(map.get(event_id), Some(&i));
			assert!(set.contains(event_id));
		}

		assert_eq!(map.len(), ids.len());
	}
}
