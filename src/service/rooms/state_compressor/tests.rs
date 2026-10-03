use std::collections::BTreeSet;

use futures::{
	StreamExt, TryFutureExt, TryStreamExt,
	future::{join_all, try_join_all},
	stream::try_unfold,
};
use ruma::events::StateEventType;
use serde_bytes::Bytes;
use tuwunel_core::{Result, config::Figment, itertools::Itertools};
use tuwunel_database::{deserialize_from_slice, map, serialize_to as ser, serialize_to_vec};

use super::{
	StateDiff, compress_state_event, parse_compressed_state_event,
	rows::{pack, unpack},
};
use crate::{Services, test_utils::fixture};

type State = BTreeSet<(u64, u64)>;

#[test]
fn rows_codec_round_trip() -> Result {
	let key = (0xFF00_FF00_FF00_FF00_u64, "m.room.name", "", u64::MAX);
	let bytes = serialize_to_vec(key)?;
	let decoded: (u64, &str, &str, u64) = deserialize_from_slice(&bytes)?;

	assert_eq!(decoded, key);
	for b in [bytes.as_slice(), &[]] {
		assert_eq!(ser::<Vec<_>, _>((key.0, Bytes::new(b)))?, ser::<Vec<_>, _>((key.0, b))?);
	}

	for meta in [&[0_u64][..], &[7, u64::MAX, 2, 3][..]] {
		assert_eq!(unpack(&pack(meta)?)?.as_slice(), meta);
	}

	for length in [0, 7, 9, 31, 33, 40] {
		unpack(&vec![0; length]).expect_err("invalid metadata length");
	}

	Ok(())
}

#[tokio::test]
async fn rows_match_blob_fold() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let kind = StateEventType::RoomMember;
	let named = services
		.short
		.get_or_create_shortstatekey(&kind, "@a:localhost")
		.await;

	let empty = services
		.short
		.get_or_create_shortstatekey(&kind, "")
		.await;

	let other = services
		.short
		.get_or_create_shortstatekey(&StateEventType::RoomName, "")
		.await;

	let hashes = [0xFF00_u64, 0xFF01, 0xFF02, 0xFF03];
	let first = [(empty, 7), (empty, u64::MAX), (named, 8), (other, 9), (u64::MAX, 99)];

	let root_removed = [(empty, 7)];
	let both_added = [(empty, 7), (empty, 10)];
	let both_removed = [(empty, 7), (named, 8)];
	let third_added = [(empty, 11)];
	let third_removed = [(empty, u64::MAX)];
	let fourth_added = [(named, 8)];
	let fourth_removed = [(empty, 10)];

	write_blob(services, hashes[0], 0, &first, &root_removed);
	write_blob(services, hashes[1], hashes[0], &both_added, &both_removed);
	write_blob(services, hashes[2], hashes[1], &third_added, &third_removed);
	write_blob(services, hashes[3], hashes[2], &fourth_added, &fourth_removed);
	let expected = try_join_all(hashes.map(|hash| blob_fold(services, hash))).await?;

	for (hash, expected) in hashes.into_iter().zip(&expected) {
		compare(services, hash, expected).await?;
		services.db[map!("shortstatehash_statediff")].remove(&hash.to_be_bytes());
	}

	for (hash, expected) in hashes.into_iter().zip(&expected) {
		compare(services, hash, expected).await?;
	}

	for ((old, new), (before, after)) in hashes
		.into_iter()
		.tuple_windows()
		.zip(expected.iter().tuple_windows())
	{
		let added: State = services
			.state_accessor
			.state_added((old, new))
			.collect()
			.await;

		let removed: State = services
			.state_accessor
			.state_removed((old, new))
			.collect()
			.await;

		assert!(added.iter().eq(after.difference(before)));
		assert!(removed.iter().eq(before.difference(after)));
	}

	Ok(())
}

fn write_blob(
	services: &Services,
	hash: u64,
	parent: u64,
	added: &[(u64, u64)],
	removed: &[(u64, u64)],
) {
	let value: Vec<_> = parent
		.to_be_bytes()
		.into_iter()
		.chain(compressed(added))
		.chain([0_u8; 8])
		.chain(compressed(removed))
		.collect();

	services.db[map!("shortstatehash_statediff")].insert(&hash.to_be_bytes(), &value);
}

async fn blob_fold(services: &Services, hash: u64) -> Result<State> {
	let layers = try_unfold(Some(hash), async |next| -> Result<_> {
		let Some(hash) = next else { return Ok(None) };
		let diff = services
			.state_compressor
			.get_statediff(hash)
			.await?;

		let parent = diff.parent;

		Ok(Some((diff, parent)))
	})
	.try_collect::<Vec<_>>()
	.await?;

	let state = layers.iter().rev().fold(State::new(), fold_layer);

	let retained = join_all(state.into_iter().map(async |record| {
		services
			.short
			.get_statekey_from_short(record.0)
			.await
			.is_ok()
			.then_some(record)
	}))
	.await;

	Ok(retained.into_iter().flatten().collect())
}

async fn compare(services: &Services, hash: u64, expected: &State) -> Result {
	let accessor = &services.state_accessor;
	let full: Vec<_> = accessor
		.state_full_shortids(hash)
		.try_collect()
		.await?;

	assert_eq!(full.iter().copied().collect::<State>(), *expected);
	expected
		.iter()
		.map(|&(key, _)| key)
		.sorted_unstable()
		.dedup()
		.for_each(|key| {
			let events = full
				.iter()
				.filter(|&&(ssk, _)| ssk == key)
				.map(|&(_, event)| event);

			assert!(events.is_sorted());
		});

	for kind in [StateEventType::RoomMember, StateEventType::RoomName] {
		let keys = accessor
			.state_keys(hash, &kind)
			.collect::<Vec<_>>()
			.await;

		let pairs = accessor
			.state_keys_with_shortids(hash, &kind)
			.collect::<Vec<_>>()
			.await;

		let wanted = try_join_all(expected.iter().map(|&(ssk, event)| {
			services
				.short
				.get_statekey_from_short(ssk)
				.map_ok(move |(kind, key)| (kind, key, event))
		}))
		.await?;

		let wanted: Vec<_> = wanted
			.into_iter()
			.filter(|(event_type, ..)| *event_type == kind)
			.map(|(_, key, event)| (key, event))
			.sorted_unstable()
			.collect();

		let pairs: Vec<_> = pairs.into_iter().sorted_unstable().collect();
		let keys: Vec<_> = keys.into_iter().sorted_unstable().collect();
		let expected_keys: Vec<_> = wanted
			.iter()
			.map(|(key, _)| key.clone())
			.collect();

		assert_eq!(pairs, wanted);
		assert_eq!(keys, expected_keys);

		for key in ["", "@a:localhost", "absent"] {
			let point = accessor
				.state_get_shortid(hash, &kind, key)
				.await
				.ok();

			let expected_event = wanted
				.iter()
				.find(|(state_key, _)| state_key.as_str() == key)
				.map(|(_, event)| *event);

			assert_eq!(point, expected_event);
		}
	}

	Ok(())
}

fn compressed(pairs: &[(u64, u64)]) -> impl Iterator<Item = u8> + '_ {
	// Descending runs model legacy hash-set serialization, including the tag-3 layer.
	pairs
		.iter()
		.sorted_unstable()
		.rev()
		.flat_map(|&(key, short)| compress_state_event(key, short))
}

fn fold_layer(mut state: State, diff: &StateDiff) -> State {
	state.extend(
		diff.added
			.iter()
			.copied()
			.map(parse_compressed_state_event),
	);

	// Legacy root removals do not contribute to the reconstructed state.
	if diff.parent.is_some() {
		diff.removed
			.iter()
			.copied()
			.map(parse_compressed_state_event)
			.for_each(|record| {
				state.remove(&record);
			});
	}

	state
}
