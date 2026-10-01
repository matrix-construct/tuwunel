use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};

use tuwunel_core::utils::math::usize_from_u64_truncated;

use super::shard;
use crate::sending::{Destination, dest::DestinationRef};

#[test]
fn borrowed_shards_preserve_destination_hashing() {
	let long = "long-server-name-exceeding-the-inline-budget.example.org"
		.try_into()
		.expect("server");

	let destinations = [
		Destination::Appservice("registration".into()),
		Destination::Push("@user:localhost".try_into().expect("user"), "key".into()),
		Destination::Federation("example.org".try_into().expect("server")),
		Destination::Federation("example.org:8448".try_into().expect("server")),
		Destination::Federation("[2001:db8::1]:8448".try_into().expect("server")),
		Destination::Federation(long),
	];

	for dest in destinations {
		let hasher = BuildHasherDefault::<DefaultHasher>::default();
		let owned = hasher.hash_one(&dest);
		let borrowed = hasher.hash_one(dest.borrowed());

		assert_eq!(owned, borrowed, "{dest:?}");

		for count in [0, 1, 2, 3, 7, 32, 64] {
			check_count(&dest, owned, count);
		}
	}
}

fn check_count(dest: &Destination, owned: u64, count: usize) {
	let owned = usize_from_u64_truncated(owned);
	let expected = match count {
		| 0 | 1 => 0,
		| _ => owned.overflowing_rem(count).0,
	};

	assert_eq!(shard(&dest.borrowed(), count), expected);

	check_federation(dest, count, expected);
}

fn check_federation(dest: &Destination, count: usize, expected: usize) {
	let Destination::Federation(server) = dest else {
		return;
	};

	assert_eq!(shard(&DestinationRef::Federation(server), count), expected);
}
