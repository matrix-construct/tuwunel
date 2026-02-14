use std::{
	ops::Range,
	time::{Duration, SystemTime},
};

use arrayvec::ArrayString;
use rand::{Rng, seq::SliceRandom, thread_rng};
use ruma::OwnedEventId;

pub fn shuffle<T>(vec: &mut [T]) {
	let mut rng = thread_rng();
	vec.shuffle(&mut rng);
}

pub fn string(length: usize) -> String {
	thread_rng()
		.sample_iter(&rand::distributions::Alphanumeric)
		.take(length)
		.map(char::from)
		.collect()
}

#[inline]
pub fn string_array<const LENGTH: usize>() -> ArrayString<LENGTH> {
	let mut ret = ArrayString::<LENGTH>::new();
	thread_rng()
		.sample_iter(&rand::distributions::Alphanumeric)
		.take(LENGTH)
		.map(char::from)
		.for_each(|c| ret.push(c));

	ret
}

#[must_use]
pub fn event_id() -> OwnedEventId {
	use base64::{
		Engine,
		alphabet::URL_SAFE,
		engine::{GeneralPurpose, general_purpose::NO_PAD},
	};

	let mut binary: [u8; 32] = [0; _];
	thread_rng().fill(&mut binary);

	let mut encoded: [u8; 43] = [0; _];
	GeneralPurpose::new(&URL_SAFE, NO_PAD)
		.encode_slice(binary, &mut encoded)
		.expect("Failed to encode binary to base64");

	let event_id: &str = str::from_utf8(&encoded)
		.expect("Failed to convert array of base64 bytes to valid utf8 str");

	OwnedEventId::from_parts('$', event_id, None)
		.expect("Failed to generate valid random event_id")
}

#[must_use]
pub fn truncate_string(mut str: String, range: Range<u64>) -> String {
	let len = thread_rng()
		.gen_range(range)
		.try_into()
		.unwrap_or(usize::MAX);

	if let Some((i, _)) = str.char_indices().nth(len) {
		str.truncate(i);
	}

	str
}

#[inline]
#[must_use]
pub fn truncate_str(str: &str, range: Range<u64>) -> &str {
	let len = thread_rng()
		.gen_range(range)
		.try_into()
		.unwrap_or(usize::MAX);

	str.char_indices()
		.nth(len)
		.map(|(i, _)| str.split_at(i).0)
		.unwrap_or(str)
}

#[inline]
#[must_use]
pub fn time_from_now_secs(range: Range<u64>) -> SystemTime {
	SystemTime::now()
		.checked_add(secs(range))
		.expect("range does not overflow SystemTime")
}

#[must_use]
pub fn secs(range: Range<u64>) -> Duration {
	let mut rng = thread_rng();
	Duration::from_secs(rng.gen_range(range))
}
