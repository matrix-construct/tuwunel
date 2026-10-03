use std::{collections::BTreeSet, sync::Arc};

use futures::{
	StreamExt, TryFutureExt, TryStreamExt,
	future::{join_all, try_join_all},
	stream::try_unfold,
};
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, RoomId, event_id,
	events::{StateEventType, TimelineEventType},
	room_id,
	serde::Raw,
	uint, user_id,
};
use serde_bytes::Bytes;
use tuwunel_core::{
	Result,
	config::Figment,
	err, expected, implement,
	itertools::Itertools,
	utils::{calculate_hash, u64_from_bytes},
};
use tuwunel_database::{deserialize_from_slice, map, serialize_to as ser, serialize_to_vec};
use tuwunel_matrix::PduEvent;

use super::{
	CompressedState, Service, StateDiff, compress_state_event, parse_compressed_state_event,
	rows::{pack, unpack},
};
use crate::{
	Services, migrations::migrations, rooms::short::ShortStateHash, test_utils::fixture,
};

type State = BTreeSet<(u64, u64)>;
type StoredRows = Vec<(Vec<u8>, Vec<u8>)>;

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

#[tokio::test]
async fn writes_match_blob_fold() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let compressor = &services.state_compressor;
	let room = room_id!("!snapshot-writers:localhost");
	let keys = join_all((0..16).map(async |index| {
		services
			.short
			.get_or_create_shortstatekey(&StateEventType::RoomMember, &index.to_string())
			.await
	}))
	.await;

	for index in 0_u64..40 {
		let event = EventId::parse(format!("$writer-{index}:localhost"))?;
		let shortevent = services
			.short
			.get_or_create_shorteventid(&event)
			.await;

		let replacement = if index == 7 {
			let removed = event_id!("$writer-5:localhost");

			services
				.short
				.get_or_create_shorteventid(removed)
				.await
		} else {
			shortevent
		};

		let expected = expected_state(&keys, index, replacement, shortevent);

		if index == 8 {
			let planted = 0x00FF_00AA_u64;

			write_blob(services, planted, 0, &[(keys[1], 30_001)], &[(keys[0], shortevent)]);
			set_room_state(services, room, planted).await;
		}

		let previous = services
			.state
			.get_room_shortstatehash(room)
			.await
			.ok();

		let compressed: CompressedState = expected
			.iter()
			.map(|&(key, short)| compress_state_event(key, short))
			.collect();

		let digest = calculate_hash(compressed.iter().map(|record| &record[..]));
		let append = index >= 9 && index.is_multiple_of(3);
		let hash = write_state(services, room, event, compressed, index).await?;

		let stored = services.db[map!("shortstatehash_statemeta")]
			.get(&hash.to_be_bytes())
			.await?;

		let meta = unpack(&stored)?;
		let diff = compressor.get_statediff(hash).await?;

		let added_len = diff.added.len();
		let removed_len = diff.removed.len();

		assert_eq!(meta[0], u64::try_from(expected!(added_len + removed_len))?);
		let ancestors: Vec<_> = try_unfold(diff.parent, async |next| -> Result<_> {
			let Some(parent) = next else { return Ok(None) };
			let diff = compressor.get_statediff(parent).await?;

			Ok(Some((parent, diff.parent)))
		})
		.try_collect()
		.await?;

		assert_eq!(&meta[1..], ancestors);
		if diff.parent.is_none() {
			assert!(diff.removed.is_empty());
		}

		match index {
			| 3 => assert_eq!(meta.len(), 4),
			| 4 => assert_ne!(diff.parent, previous, "fifth layer folds"),
			| 5 | 8 => assert!(diff.parent.is_none(), "large delta folds into root"),
			| _ => {},
		}

		if !append {
			assert_eq!(services.short.get_shortstatehash(&digest).await?, hash);
		}

		assert_eq!(blob_fold(services, hash).await?, expected);
		compare(services, hash, &expected).await?;
		set_room_state(services, room, hash).await;
	}

	Ok(())
}

fn expected_state(keys: &[u64], index: u64, replacement: u64, shortevent: u64) -> State {
	let base = if index < 5 { 10_000 } else { 30_000 };

	keys.iter()
		.copied()
		.enumerate()
		.filter(|&(offset, _)| index != 6 || offset != 0)
		.map(|(offset, key)| {
			let event_offset = u64::try_from(offset).expect("small fixture");
			let short = match offset {
				| 0 => replacement,
				| 1 if index == 7 => shortevent,
				| _ => expected!(base + event_offset),
			};

			(key, short)
		})
		.collect()
}

async fn write_state(
	services: &Services,
	room: &RoomId,
	event: OwnedEventId,
	compressed: CompressedState,
	index: u64,
) -> Result<u64> {
	let via_event = matches!(index, 1..=4 | 7) || index >= 9 && index % 3 == 1;

	match index {
		| _ if via_event =>
			services
				.state
				.set_event_state(&event, room, Arc::new(compressed))
				.await,
		| _ if index >= 9 && index.is_multiple_of(3) => {
			let pdu = writer_pdu(event, room)?;

			services.state.append_to_state(&pdu).await
		},
		| _ =>
			services
				.state_compressor
				.save_state(room, Arc::new(compressed))
				.map_ok(|saved| saved.shortstatehash)
				.await,
	}
}

async fn set_room_state(services: &Services, room: &RoomId, hash: u64) {
	let guard = services.state.mutex.lock(room).await;

	services.state.set_room_state(room, hash, &guard);
}

fn writer_pdu(event: OwnedEventId, room: &RoomId) -> Result<PduEvent> {
	let pdu = PduEvent {
		kind: TimelineEventType::RoomMember,
		content: Raw::new(&CanonicalJsonObject::new())?,
		event_id: event,
		room_id: room.to_owned(),
		sender: user_id!("@writer:localhost").to_owned(),
		state_key: Some("0".into()),
		redacts: None,
		prev_events: Default::default(),
		auth_events: Default::default(),
		origin_server_ts: uint!(0),
		depth: uint!(1),
		hashes: Default::default(),
		origin: None,
		unsigned: None,
	};

	Ok(pdu)
}

#[tokio::test]
async fn population_matches_read_repair() -> Result {
	let config = Figment::new().merge(("create_admin_room", false));
	let Some(fixture) = fixture(config).await? else { return Ok(()) };
	let services = &fixture.services;
	let global = &services.db[map!("global")];
	let marker = b"populate_snapshot_rows";
	let repair_marker = b"repair_short_injectivity_seen";
	let blobs = &services.db[map!("shortstatehash_statediff")];
	let meta = &services.db[map!("shortstatehash_statemeta")];
	let delta = &services.db[map!("shortstatehash_statedelta")];

	migrate_rows(services).await?;
	services
		.users
		.create(user_id!("@population:localhost"), None, None)
		.await?;

	let key = services
		.short
		.get_or_create_shortstatekey(&StateEventType::RoomName, "")
		.await;

	let event = services
		.short
		.get_or_create_shorteventid(event_id!("$population:localhost"))
		.await;

	write_blob(services, 900, 0, &[(key, event), (u64::MAX, event)], &[(key, event)]);
	write_blob(services, 200, 900, &[(key, u64::MAX)], &[(key, u64::MAX)]);
	write_blob(services, 300, 200, &[(key, event)], &[]);
	write_blob(services, 400, 300, &[(key, u64::MAX)], &[]);
	repair_by_reading(services, 400).await?;

	let repaired = stored_rows(services).await?;
	let damaged = 902_u64.to_be_bytes();

	blobs.insert(&damaged, [0; 3]);

	plant_stale_rows(services, event)?;
	global.remove(marker);
	migrate_rows(services).await?;
	meta.get(&damaged)
		.await
		.expect_err("damaged snapshot has no meta");

	assert!(
		delta
			.raw_stream_prefix(&damaged)
			.boxed()
			.try_next()
			.await?
			.is_none()
	);

	assert_eq!(stored_rows(services).await?, repaired);
	migrate_rows(services).await?;
	assert_eq!(stored_rows(services).await?, repaired, "completed rerun changes nothing");

	blobs.clear().await;
	write_blob(services, 900, 0, &[(key, event)], &[]);
	services.db[map!("statehash_shortstatehash")].insert(b"retained", 900_u64.to_be_bytes());
	meta.clear().await;
	delta.clear().await;
	repair_by_reading(services, 900).await?;

	let healthy = stored_rows(services).await?;

	assert!(
		global.get(marker).await.is_ok(),
		"rebuild cannot rely on missing population marker"
	);

	plant_stale_rows(services, event)?;
	global.insert(repair_marker, b"unknown");
	migrate_rows(services).await?;
	assert_ne!(global.get(repair_marker).await?.as_ref(), b"unknown");

	assert_eq!(stored_rows(services).await?, healthy);

	Ok(())
}

async fn migrate_rows(services: &Services) -> Result {
	migrations(services).await?;
	services.db[map!("global")]
		.get("populate_snapshot_rows")
		.await
		.expect("population stamped");

	Ok(())
}

async fn repair_by_reading(services: &Services, hash: u64) -> Result {
	services
		.state_compressor
		.rows(hash, None, None)
		.await?
		.count();

	Ok(())
}

fn plant_stale_rows(services: &Services, event: u64) -> Result {
	let stale = serialize_to_vec((901_u64, "m.room.name", "", event))?;

	services.db[map!("shortstatehash_statemeta")].insert(&901_u64.to_be_bytes(), pack(&[1])?);
	services.db[map!("shortstatehash_statedelta")].insert(&stale, [1_u8; 9]);
	Ok(())
}

async fn stored_rows(services: &Services) -> Result<Vec<StoredRows>> {
	try_join_all(
		[
			&services.db[map!("shortstatehash_statemeta")],
			&services.db[map!("shortstatehash_statedelta")],
		]
		.map(async |map| {
			map.raw_stream()
				.map_ok(|(key, value)| (key.to_vec(), value.to_vec()))
				.try_collect()
				.await
		}),
	)
	.await
}

/// Reads one state's delta row into its typed form.
///
/// Rows round-trip through [`save_statediff`], the pair being the only
/// codec for the statediff encoding.
///
/// # Panics
///
/// Panics if a stored delta row is shorter than its eight-byte parent prefix.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug", name = "get")]
pub(crate) async fn get_statediff(&self, shortstatehash: ShortStateHash) -> Result<StateDiff> {
	const BUFSIZE: usize = size_of::<ShortStateHash>();
	const STRIDE: usize = size_of::<ShortStateHash>();

	let value = self
		.db
		.shortstatehash_statediff
		.aqry::<BUFSIZE, _>(&shortstatehash)
		.await
		.map_err(|e| {
			err!(Database("Failed to find StateDiff from short {shortstatehash:?}: {e}"))
		})?;

	let parent = u64_from_bytes(&value[0..size_of::<u64>()])
		.ok()
		.take_if(|parent| *parent != 0);

	debug_assert!(value.len().is_multiple_of(STRIDE), "value not aligned to stride");
	let _num_values = value.len() / STRIDE;

	let mut add_mode = true;
	let mut added = CompressedState::new();
	let mut removed = CompressedState::new();

	let mut i = STRIDE;
	while let Some(v) = value.get(i..expected!(i + 2 * STRIDE)) {
		if add_mode && v.starts_with(&0_u64.to_be_bytes()) {
			add_mode = false;
			i = expected!(i + STRIDE);
			continue;
		}

		if add_mode {
			added.insert(v.try_into()?);
		} else {
			removed.insert(v.try_into()?);
		}

		i = expected!(i + 2 * STRIDE);
	}

	Ok(StateDiff {
		parent,
		added: Arc::new(added),
		removed: Arc::new(removed),
	})
}
