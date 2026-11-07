use std::{
	collections::{BTreeMap, BTreeSet},
	iter::once,
};

use futures::{TryFutureExt, TryStreamExt};
use tuwunel_core::{
	Err, Result, err,
	itertools::Itertools,
	smallvec::SmallVec,
	utils::{
		IterStream, TryReadyExt,
		hash::sha256::{concat, hash},
		stream::TryBroadbandExt,
	},
};
use tuwunel_database::{Database, Txn, TxnError};

use super::identity::clear_cache;
use crate::{
	Services,
	integrity::{
		ScanExt,
		short_ids::{
			identity::{Identities, census as census_identities},
			state::{
				Aliases, Mapping, Materialized, Parents, Projection, ancestors, claim, decode,
				depth, descendants, fetch,
				history::{
					References as Historical, references as history_references,
					target as historical_target,
				},
				mapping, materialize, present, project, survey,
			},
		},
	},
	rooms::state_compressor::CompressedState,
};

pub(super) mod gc;
mod orphan;
#[cfg(test)]
mod tests;

use self::orphan::recover;

type Claimed = BTreeMap<u64, Digests>;
type Digests = SmallVec<[Digest; 1]>;
type Digest = (SmallVec<[u8; 32]>, SmallVec<[u8; 8]>);

const PARENTLESS: [u8; 8] = [0; 8];

pub(super) struct States {
	pub(super) unfinished: BTreeSet<u64>,
	pub(super) rewritten: u64,
	pub(super) original_bytes: u64,
	pub(super) snapshot_bytes: u64,
	pub(super) uncertain: bool,
	pub(super) restored: bool,
}

struct Target {
	original: [u8; 32],
	snapshot: [u8; 32],
}

#[derive(Clone, Copy)]
enum Operation {
	Publish,
	Restore,
}

enum Publication {
	Applied(usize, usize),
	Retained,
	Restored,
}

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn repair(
	services: &Services,
	identities: Identities,
) -> Result<(States, Identities)> {
	let census = survey(services, &identities).await?;
	let affected = descendants(&census.parents, &census.seeds);
	let uncertain = census.unknown || !census.held.is_empty();
	let pending = |unfinished| States {
		unfinished,
		rewritten: 0,
		original_bytes: 0,
		snapshot_bytes: 0,
		uncertain,
		restored: false,
	};

	if census.unknown {
		return Ok((pending(affected), identities));
	}

	// The summaries clear before any snapshot or reinstated reverse row is written.
	if !affected.is_empty() || !census.orphans.is_empty() {
		clear_cache(services, "roomid_spacehierarchy").await?;
	}

	let recovered = recover(services, &census).await?;
	let identities = match recovered.is_empty() {
		| true => identities,
		| false => census_identities(services).await?,
	};

	let projection = match recovered.is_empty() {
		| true => census.projection,
		| false => {
			let statekeys = mapping(services, &identities.statekeys).await?;

			with_recovered(census.projection, &recovered, statekeys)
		},
	};

	let digests = digests(services, &affected).await?;
	let historical = descendants(&census.parents, &census.exposed);
	let history = history_references(services, historical).await?;

	// Serial: each target holds a full state in memory, and history fans out within it.
	let targets: BTreeMap<_, _> = affected
		.iter()
		.copied()
		.try_stream()
		.and_then(async |id| {
			services.server.check_running()?;
			target(services, &projection, &history, id)
				.map_ok(|target| (id, target))
				.await
		})
		.try_collect()
		.await?;

	let blocked = targets
		.iter()
		.filter(|(_, target)| target.is_none())
		.map(|(id, _)| *id)
		.chain(census.held.iter().copied())
		.flat_map(|id| ancestors(&census.parents, id))
		.collect();

	let order = affected
		.iter()
		.filter_map(|id| depth(&census.parents, *id).map(|depth| (depth, *id)))
		.sorted_unstable()
		.rev();

	let publish_next = async |(states, blocked): (States, BTreeSet<u64>),
	                          (_, id): (usize, u64)| {
		if blocked.contains(&id) {
			return Ok((states, blocked));
		}

		let Some(Some(target)) = targets.get(&id) else {
			return Ok((states, block(blocked, &census.parents, id)));
		};

		let digests = digests
			.get(&id)
			.map(Digests::as_slice)
			.unwrap_or_default();

		publish(services, &projection, &history, id, target, digests)
			.map_ok(|published| match published {
				| Publication::Applied(original, snapshot) =>
					(accepted(states, id, original, snapshot), blocked),
				| Publication::Retained => (states, block(blocked, &census.parents, id)),
				| Publication::Restored =>
					(restored(states), block(blocked, &census.parents, id)),
			})
			.await
	};

	order
		.try_stream()
		.try_fold((pending(affected), blocked), publish_next)
		.map_ok(|(states, _)| (states, identities))
		.await
}

fn block(mut blocked: BTreeSet<u64>, parents: &Parents, id: u64) -> BTreeSet<u64> {
	blocked.extend(ancestors(parents, id));
	blocked
}

#[tracing::instrument(level = "trace", skip_all)]
async fn digests(services: &Services, affected: &BTreeSet<u64>) -> Result<Claimed> {
	if affected.is_empty() {
		return Ok(Claimed::new());
	}

	let gather = |mut digests: Claimed, (key, value): (&[u8], &[u8])| {
		let Some(id) = claim(value).filter(|id| affected.contains(id)) else {
			return Ok(digests);
		};

		digests
			.entry(id)
			.or_default()
			.push((SmallVec::from_slice(key), SmallVec::from_slice(value)));

		Ok(digests)
	};

	services.db["statehash_shortstatehash"]
		.raw_stream()
		.scanned(&services.server)
		.ready_try_fold(Claimed::new(), gather)
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn target(
	services: &Services,
	projection: &Projection,
	history: &Historical,
	id: u64,
) -> Result<Option<Target>> {
	prepared(services, projection, history, id)
		.map_ok(|prepared| {
			prepared.map(|(original, state)| Target {
				original: hash(original),
				snapshot: fingerprint(&state),
			})
		})
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn prepared(
	services: &Services,
	projection: &Projection,
	history: &Historical,
	id: u64,
) -> Result<Materialized> {
	if history.affected.contains(&id) {
		return historical_target(services, projection, history, id).await;
	}

	let Some((original, state)) = materialize(services, id).await? else {
		return Ok(None);
	};

	Ok(project(state, projection).map(|state| (original, state)))
}

fn fingerprint(state: &CompressedState) -> [u8; 32] {
	concat(once(PARENTLESS.as_slice()).chain(state.iter().map(AsRef::as_ref)))
}

fn with_recovered(
	mut projection: Projection,
	recovered: &Aliases,
	statekeys: Mapping,
) -> Projection {
	projection
		.statekeys
		.refused
		.retain(|short| !recovered.contains_key(short));

	projection
		.statekeys
		.refused
		.extend(statekeys.refused);

	projection.statekeys.aliases = statekeys.aliases;

	projection
}

#[tracing::instrument(level = "trace", skip_all)]
async fn publish(
	services: &Services,
	projection: &Projection,
	history: &Historical,
	id: u64,
	target: &Target,
	digests: &[Digest],
) -> Result<Publication> {
	services.server.check_running()?;
	let db = &services.db;
	let Some((original, state)) = prepared(services, projection, history, id).await? else {
		return Ok(Publication::Retained);
	};

	let snapshot = encode(&state)?;

	decode(&snapshot)
		.filter(|diff| diff.parent.is_none() && diff.removed.is_empty() && diff.added == state)
		.ok_or_else(|| err!("snapshot prepublication verification failed"))?;

	if hash(&original) != target.original || hash(&snapshot) != target.snapshot {
		return Err!("snapshot target changed after preflight");
	}

	if !digests_match(db, digests).await? {
		return Err!("snapshot digest changed after preflight");
	}

	commit(services, transaction(db, id, &snapshot, digests, Operation::Publish))
		.map_err(|error| err!("{error}"))?;

	if holds(db, id, &snapshot).await? && digests_absent(db, digests).await? {
		return Ok(Publication::Applied(original.len(), snapshot.len()));
	}

	commit(services, transaction(db, id, &original, digests, Operation::Restore))
		.map_err(|error| err!("snapshot restoration failed: {error}"))?;

	if !holds(db, id, &original).await? {
		return Err!("snapshot restoration verification failed");
	}

	if !digests_match(db, digests).await? {
		return Err!("digest restoration verification failed");
	}

	Ok(Publication::Restored)
}

fn encode(state: &CompressedState) -> Result<Vec<u8>> {
	let capacity = state
		.len()
		.checked_mul(16)
		.and_then(|size| size.checked_add(8))
		.ok_or_else(|| err!("snapshot size overflow"))?;

	encoded(state, capacity)
}

fn encoded(state: &CompressedState, capacity: usize) -> Result<Vec<u8>> {
	let mut bytes = Vec::new();

	bytes
		.try_reserve_exact(capacity)
		.map_err(|error| err!("{error}"))?;

	bytes.extend(PARENTLESS);
	bytes.extend(state.iter().flatten());
	Ok(bytes)
}

#[tracing::instrument(level = "trace", skip_all)]
async fn digests_match(db: &Database, digests: &[Digest]) -> Result<bool> {
	digests
		.try_stream()
		.broad_try_all(|(key, value)| {
			db["statehash_shortstatehash"]
				.get(key)
				.map_ok(move |current| current.as_ref() == value.as_slice())
		})
		.await
}

fn transaction(
	db: &Database,
	id: u64,
	bytes: &[u8],
	digests: &[Digest],
	operation: Operation,
) -> Txn {
	let statehashes = &db["statehash_shortstatehash"];
	let txn = digests
		.iter()
		.fold(db.txn(), |mut txn, (key, value)| {
			match operation {
				| Operation::Publish => txn.del_raw(statehashes, key),
				| Operation::Restore => txn.insert_raw(statehashes, key, value),
			}

			txn
		});

	once((id.to_be_bytes(), bytes)).fold(txn, |mut txn, (key, bytes)| {
		txn.insert_raw(&db["shortstatehash_statediff"], key, bytes);
		txn
	})
}

fn commit(services: &Services, txn: Txn) -> Result<(), TxnError> {
	// Acquire before acceptance, then clear even on accepted-write sync failure.
	let mut cache = services
		.state_compressor
		.stateinfo_cache
		.lock()
		.map_err(|_| TxnError::Write(err!("state cache lock poisoned")))?;

	let written = txn.try_execute();

	if !matches!(written, Err(TxnError::Write(_))) {
		cache.clear();
	}

	written
}

#[tracing::instrument(level = "trace", skip_all)]
async fn holds(db: &Database, id: u64, bytes: &[u8]) -> Result<bool> {
	fetch(db, "shortstatehash_statediff", &id.to_be_bytes(), |value| value == bytes)
		.map_ok(Option::unwrap_or_default)
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn digests_absent(db: &Database, digests: &[Digest]) -> Result<bool> {
	digests
		.try_stream()
		.broad_try_all(|(key, _)| {
			present(db, "statehash_shortstatehash", key).map_ok(|found| !found)
		})
		.await
}

fn accepted(mut states: States, id: u64, original_len: usize, snapshot_len: usize) -> States {
	states.unfinished.remove(&id);
	states.rewritten = states.rewritten.saturating_add(1);
	states.original_bytes = states
		.original_bytes
		.saturating_add(u64::try_from(original_len).unwrap_or(u64::MAX));

	states.snapshot_bytes = states
		.snapshot_bytes
		.saturating_add(u64::try_from(snapshot_len).unwrap_or(u64::MAX));

	states
}

fn restored(states: States) -> States { States { restored: true, ..states } }
