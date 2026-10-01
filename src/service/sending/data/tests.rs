use std::sync::atomic::{AtomicUsize, Ordering};

use futures::{StreamExt, future::ready};
use ruma::ServerName;
use tuwunel_core::{Error, Result, config::Figment, itertools::Itertools, utils::TryReadyExt};

use super::{Data, Key, queued_destinations, replace_key};
use crate::{sending::Destination, test_utils::fixture};

#[derive(Default)]
struct Counts {
	seeks: AtomicUsize,
	keys: AtomicUsize,
	max_seeks: Option<usize>,
	max_keys: Option<usize>,
}

#[tokio::test]
async fn discovery_seek_counts_ignore_queue_depth_and_shard_filtering() -> Result {
	let count = |(found, owned): (usize, bool), dest: Destination| {
		let owned =
			owned && matches!(&dest, Destination::Federation(server) if owns_even(server));

		Ok((found.saturating_add(1), owned))
	};

	for destinations in [0, 4, 1_800] {
		for depth in [1, 64] {
			let rows = rows(destinations, depth);
			let counts = Counts {
				max_seeks: Some(destinations + 3),
				max_keys: Some(destinations + 2),
				..Default::default()
			};

			let seek = |lower| ready(Ok(counted_key(&rows, &counts, lower)));
			let (found, owned) = queued_destinations(seek, owns_even)
				.ready_try_fold((0_usize, true), count)
				.await?;

			assert_eq!(found, destinations.div_ceil(2));
			assert!(owned);

			assert_counts(&counts, destinations);
			eprintln!(
				"discovery destinations={destinations} depth={depth} rows={} keys={} seeks={}",
				rows.len(),
				counts.keys.load(Ordering::Relaxed),
				counts.seeks.load(Ordering::Relaxed),
			);
		}
	}

	Ok(())
}

fn rows(destinations: usize, depth: usize) -> Vec<Key> {
	let federation = (0..destinations).flat_map(|index| {
		let name = format!("server{index:05}.example");
		let prefix = append_delimiter(name.into_bytes());

		(0..depth).map(move |row| row_key(&prefix, row))
	});

	let excluded = [b"$@user:localhost\xffkey\xff".as_slice(), b"+bridge\xff"]
		.into_iter()
		.flat_map(|prefix| (0..depth).map(move |row| row_key(prefix, row)));

	federation
		.chain(excluded)
		.sorted_unstable()
		.collect()
}

fn owns_even(server: &ServerName) -> bool {
	server
		.as_str()
		.strip_prefix("server")
		.and_then(|name| name.strip_suffix(".example"))
		.and_then(|index| index.parse().ok())
		.is_some_and(|index: usize| index.is_multiple_of(2))
}

fn append_delimiter(mut key: Key) -> Key {
	key.push(u8::MAX);
	key
}

fn delimited(bytes: &[u8]) -> Key { bytes.iter().copied().chain([u8::MAX]).collect() }

fn row_key(prefix: &[u8], row: usize) -> Key {
	let count = u64::try_from(row)
		.expect("row count")
		.to_be_bytes();

	prefix.iter().chain(&count).copied().collect()
}

fn counted_key(rows: &[Key], counts: &Counts, lower: Key) -> Option<Key> {
	count_seek(counts);
	let index = rows.partition_point(|row| row < &lower);

	rows.get(index).map(|row| {
		count_key(counts);
		copied_key(lower, row)
	})
}

fn count_seek(counts: &Counts) {
	let count = counts
		.seeks
		.fetch_add(1, Ordering::Relaxed)
		.checked_add(1)
		.expect("seek count overflow");

	let bounded = counts
		.max_seeks
		.is_none_or(|limit| count <= limit);

	assert!(bounded, "seek bound exceeded: {count}");
}

fn count_key(counts: &Counts) {
	let count = counts
		.keys
		.fetch_add(1, Ordering::Relaxed)
		.checked_add(1)
		.expect("key count overflow");

	assert!(
		counts.max_keys.is_none_or(|limit| count <= limit),
		"key bound exceeded: {count}"
	);
}

fn copied_key(mut lower: Key, row: &[u8]) -> Key {
	replace_key(&mut lower, row);
	lower
}

fn assert_counts(counts: &Counts, destinations: usize) {
	let keys = destinations
		.checked_add(2)
		.expect("key bound overflow");

	let seeks = destinations
		.checked_add(3)
		.expect("seek bound overflow");

	assert_eq!(counts.keys.load(Ordering::Relaxed), keys);
	assert_eq!(counts.seeks.load(Ordering::Relaxed), seeks);
}

#[tokio::test]
async fn discovery_advances_past_malformed_keys_and_reports_errors() {
	let rows = [
		b"bad name\xff\x01".to_vec(),
		b"missing".to_vec(),
		b"valid.example\xff\x01".to_vec(),
		vec![255, 255],
	];

	let counts = Counts {
		max_seeks: Some(rows.len()),
		max_keys: Some(rows.len()),
		..Default::default()
	};

	let seek = |lower| ready(Ok(counted_key(&rows, &counts, lower)));
	let found: Vec<_> = queued_destinations(seek, |_| true)
		.collect()
		.await;

	assert_eq!(found.len(), 4);
	found[0]
		.as_ref()
		.expect_err("invalid server name");

	found[1]
		.as_ref()
		.expect_err("missing destination delimiter");

	assert!(
		matches!(&found[2], Ok(Destination::Federation(server)) if server.as_str() == "valid.example")
	);

	found[3]
		.as_ref()
		.expect_err("invalid destination encoding");

	assert_eq!(counts.keys.load(Ordering::Relaxed), 4);
	assert_eq!(counts.seeks.load(Ordering::Relaxed), 4);
}

#[tokio::test]
async fn discovery_stops_after_a_storage_error() {
	let counts = Counts { max_seeks: Some(1), ..Default::default() };
	let seek = |_| {
		count_seek(&counts);
		ready(Err(Error::bad_database("counted seek failed")))
	};

	let found: Vec<_> = queued_destinations(seek, |_| true)
		.collect()
		.await;

	assert_eq!(found.len(), 1);
	found[0]
		.as_ref()
		.expect_err("storage error must be visible");

	assert_eq!(counts.seeks.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn discovery_stays_lazy_when_only_one_destination_is_requested() -> Result {
	let rows = rows(32, 64);
	let counts = Counts {
		max_keys: Some(3),
		max_seeks: Some(3),
		..Default::default()
	};

	let seek = |lower| ready(Ok(counted_key(&rows, &counts, lower)));
	let found = queued_destinations(seek, |_| true)
		.take(1)
		.ready_try_fold(0_usize, |found, _| Ok(found.saturating_add(1)))
		.await?;

	assert_eq!(found, 1);
	assert_eq!(counts.keys.load(Ordering::Relaxed), 3);
	assert_eq!(counts.seeks.load(Ordering::Relaxed), 3);

	Ok(())
}

#[tokio::test]
async fn real_queue_discovery_preserves_prefix_order_and_exhaustion() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let data = &fixture.services.sending.db;
	let names = [
		"a.example",
		"a.example:8448",
		"a.example.long",
		"a0.example",
		"[2001:db8::1]",
		"[2001:db8::1]:8448",
		"z.example",
	];

	let rows = names
		.iter()
		.map(|name| delimited(name.as_bytes()))
		.flat_map(|prefix| (0..64).map(move |row| row_key(&prefix, row)))
		.chain([row_key(b"$@user:localhost\xffkey\xff", 0), row_key(b"+bridge\xff", 0)]);

	{
		let _cork = data.db.cork();

		for key in rows {
			data.servernameevent_data
				.insert(&key, b"unusable event value");
		}
	}

	let counts = Counts {
		max_keys: Some(names.len() + 2),
		max_seeks: Some(names.len() + 3),
		..Default::default()
	};

	let seek = |lower| counted_seek(data, &counts, lower);
	let expected = ordered_destinations(names);
	let (expected, found, equal) = queued_destinations(seek, |_| true)
		.ready_try_fold((expected, 0_usize, true), |(mut expected, found, equal), dest| {
			let matches = expected
				.next()
				.is_some_and(|expected| expected == dest);

			Ok((expected, found.saturating_add(1), equal && matches))
		})
		.await?;

	let remaining = expected.count();

	assert_eq!(found, names.len());
	assert_eq!(remaining, 0);
	assert!(equal);
	assert_counts(&counts, names.len());
	let destinations = data
		.queued_federation_destinations(|_| true)
		.count()
		.await;

	assert_eq!(destinations, names.len());

	Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn counted_seek(data: &Data, counts: &Counts, lower: Key) -> Result<Option<Key>> {
	count_seek(counts);
	let key = data
		.seek_queued_key(lower)
		.await?
		.inspect(|_| count_key(counts));

	Ok(key)
}

fn ordered_destinations(
	names: impl IntoIterator<Item = impl AsRef<str>>,
) -> impl Iterator<Item = Destination> {
	names
		.into_iter()
		.map(|name| {
			let dest = Destination::Federation(name.as_ref().try_into().expect("server"));
			let prefix = dest.get_prefix();

			(prefix, dest)
		})
		.sorted_unstable_by(|a, b| a.0.cmp(&b.0))
		.map(|(_, dest)| dest)
}
