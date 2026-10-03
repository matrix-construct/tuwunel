use tuwunel_core::{Result, config::Figment};
use tuwunel_database::Database;

use super::{ROOTS, collect, inspect, row};
use crate::test_utils::fixture;

#[tokio::test]
async fn fresh_roots_protect_ancestors_and_unreachable_children_are_collected() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let statediff = &db["shortstatehash_statediff"];
	let roots = [
		(ROOTS[0], &b"!room:example.org"[..]),
		(ROOTS[1], &90_u64.to_be_bytes()[..]),
		(ROOTS[2], &b"$event:example.org"[..]),
		(ROOTS[3], &[1_u8; 32][..]),
	];

	let chains = [(1_u64, 2_u64), (3, 4), (5, 6), (7, 8)];

	for ((column, key), (parent, child)) in roots.into_iter().zip(chains) {
		statediff.insert(&parent.to_be_bytes(), 0_u64.to_be_bytes());
		statediff.insert(&child.to_be_bytes(), parent.to_be_bytes());
		db[column].insert(key, child.to_be_bytes());
	}

	unreachable_chain(db);

	assert_eq!(inspect(services).await?.unfinished.len(), 3);
	let collected = collect(services).await?;

	assert_eq!(collected.deleted, 3);
	assert!(collected.unfinished.is_empty());
	assert!(!collected.unknown);
	assert_retained(db, 1..=8).await?;
	assert_collected(db, [10, 20, 30]).await?;

	for ((column, key), (_, child)) in roots.into_iter().zip(chains) {
		db[column].insert(key, [child.to_be_bytes().as_slice(), b"tail"].concat());
	}

	unreachable_chain(db);

	let collected = collect(services).await?;

	assert_eq!(collected.deleted, 3);
	assert!(collected.unknown, "readable malformed root tails remain unfinished");
	assert_retained(db, 1..=8).await?;
	assert_collected(db, [10, 20, 30]).await?;

	statediff.insert(&40_u64.to_be_bytes(), 41_u64.to_be_bytes());
	statediff.insert(&41_u64.to_be_bytes(), 40_u64.to_be_bytes());
	assert!(collect(services).await?.unknown, "a valid-framed cycle remains unfinished");
	assert_retained(db, [40, 41]).await?;

	statediff.insert(&30_u64.to_be_bytes(), 0_u64.to_be_bytes());
	statediff.insert(&20_u64.to_be_bytes(), [20_u64.to_be_bytes(), [1; 8]].concat());
	statediff.insert(b"invalid", 30_u64.to_be_bytes());
	assert_eq!(
		collect(services).await?.deleted,
		0,
		"readable parent of an invalid key retains its ancestor"
	);

	assert!(row(db, 30).await?.is_some());
	assert!(inspect(services).await?.unknown);

	statediff.remove(b"invalid");
	db[ROOTS[3]].insert(b"unknown", b"short");
	let collected = collect(services).await?;

	assert!(collected.unknown);
	assert_eq!(collected.deleted, 0);
	assert!(row(db, 30).await?.is_some());
	Ok(())
}

fn unreachable_chain(db: &Database) {
	for (id, parent) in [(30_u64, 0_u64), (20, 30), (10, 20)] {
		db["shortstatehash_statediff"].insert(&id.to_be_bytes(), parent.to_be_bytes());
	}
}

async fn assert_retained(db: &Database, ids: impl IntoIterator<Item = u64>) -> Result {
	for id in ids {
		assert!(row(db, id).await?.is_some(), "shortstatehash {id} is retained");
	}

	Ok(())
}

async fn assert_collected(db: &Database, ids: impl IntoIterator<Item = u64>) -> Result {
	for id in ids {
		assert!(row(db, id).await?.is_none(), "shortstatehash {id} is collected");
	}

	Ok(())
}
