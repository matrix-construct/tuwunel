use std::{
	ops::Range,
	time::{Duration, SystemTime},
};

use rand::{Rng, seq::SliceRandom, thread_rng};

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
