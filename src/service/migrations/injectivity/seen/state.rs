use std::{
	collections::{BTreeMap, BTreeSet},
	iter::{once, repeat},
};

use futures::{FutureExt, TryFutureExt, TryStreamExt, future::try_join, stream::unfold};
use ruma::events::TimelineEventType;
use tuwunel_core::{
	Err, Result, err, implement,
	itertools::Itertools,
	smallvec::SmallVec,
	utils::{
		BoolExt, IterStream, TryReadyExt,
		hash::sha256::{concat, hash},
		result::NotFound,
		stream::TryBroadbandExt,
	},
};
use tuwunel_database::{Database, SEP, Txn, TxnError, keyval::ValBuf};

use super::{
	Identity,
	identity::{
		Candidate, Family, Identities, Kind, admitted, census as census_identities, clear_cache,
	},
};
use crate::{
	Services,
	migrations::injectivity::scan::short_of,
	rooms::state_compressor::{
		CompressedState, CompressedStateEvent, compress_state_event, parse_compressed_state_event,
	},
};

mod history;
mod orphan;
#[cfg(test)]
mod tests;

use self::{
	history::{
		References as Historical, references as history_references, target as historical_target,
	},
	orphan::recover,
};

type Claimed = BTreeMap<u64, Digests>;
type Digests = SmallVec<[Digest; 1]>;
type Digest = (SmallVec<[u8; 32]>, SmallVec<[u8; 8]>);
type Parents = BTreeMap<u64, Option<u64>>;
type Aliases = BTreeMap<u64, (u64, Identity)>;
type Occurrences = BTreeMap<u64, BTreeSet<u64>>;
type Absences = Vec<Absence>;
type Absence = (u64, u64, bool, bool);
type Materialized = Option<(ValBuf, CompressedState)>;

const PARENTLESS: [u8; 8] = [0; 8];

struct Mapping {
	aliases: Aliases,
	refused: BTreeSet<u64>,
}

struct Projection {
	statekeys: Mapping,
	events: Mapping,
}

pub(super) struct States {
	pub(super) unfinished: BTreeSet<u64>,
	pub(super) rewritten: u64,
	pub(super) original_bytes: u64,
	pub(super) snapshot_bytes: u64,
	pub(super) uncertain: bool,
	pub(super) restored: bool,
}

struct Census {
	parents: Parents,
	seeds: BTreeSet<u64>,
	unknown: bool,
	held: Vec<u64>,
	orphans: Occurrences,
	complete: bool,
	projection: Projection,
	exposed: BTreeSet<u64>,
}

struct Scanned {
	id: Option<u64>,
	parent: Option<u64>,
	diff: Option<Diff>,
	absences: Absences,
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

struct Diff {
	parent: Option<u64>,
	added: CompressedState,
	removed: CompressedState,
}

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn repair(services: &Services, identities: &Identities) -> Result<States> {
	let census = survey(services, identities).await?;
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
		return Ok(pending(affected));
	}

	// The summaries clear before any snapshot or reinstated reverse row is written.
	if !affected.is_empty() || !census.orphans.is_empty() {
		clear_cache(services, "roomid_spacehierarchy").await?;
	}

	let recovered = recover(services, &census).await?;

	let projection = match recovered.is_empty() {
		| true => census.projection,
		| false => {
			let identities = census_identities(services).await?;
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

	let (states, _) = order
		.try_stream()
		.try_fold((pending(affected), blocked), publish_next)
		.await?;

	if states.restored {
		return Err!("snapshot verification failed; original data restored");
	}

	Ok(states)
}

#[tracing::instrument(level = "trace", skip_all)]
async fn survey(services: &Services, identities: &Identities) -> Result<Census> {
	let statekeys = mapping(services, &identities.statekeys).await?;
	let events = mapping(services, &identities.events).await?;

	census(services, Projection { statekeys, events }).await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn census(services: &Services, projection: Projection) -> Result<Census> {
	let initial = Census {
		parents: Parents::new(),
		seeds: BTreeSet::new(),
		unknown: false,
		held: Vec::new(),
		orphans: Occurrences::new(),
		complete: true,
		projection,
		exposed: BTreeSet::new(),
	};

	services.db["shortstatehash_statediff"]
		.raw_stream()
		.ready_and_then(|(key, value)| {
			services.server.check_running()?;
			services.server.progress.advance();

			Ok(Scanned {
				id: short_of(key),
				parent: value.get(..8).and_then(short_of),
				diff: decode(value),
				absences: Absences::new(),
			})
		})
		// Serial: a row's entries already fan out, and that fan-out is the concurrency budget.
		.try_fold(initial, async |census, scanned| tally(census, scan(&services.db, scanned).await?))
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn scan(db: &Database, scanned: Scanned) -> Result<Scanned> {
	let Some(diff) = scanned.diff.as_ref() else {
		return Ok(scanned);
	};

	let absences = diff
		.added
		.iter()
		.chain(&diff.removed)
		.copied()
		.try_stream()
		.broad_and_then(|entry| absence(db, entry))
		.ready_try_filter(|(_, _, key_missing, event_missing)| *key_missing || *event_missing)
		.try_collect()
		.await?;

	Ok(Scanned { absences, ..scanned })
}

#[tracing::instrument(level = "trace", skip_all)]
async fn absence(db: &Database, entry: CompressedStateEvent) -> Result<Absence> {
	let (key, event) = parse_compressed_state_event(entry);
	let (key_present, event_present) = try_join(
		present(db, "shortstatekey_statekey", &key.to_be_bytes()),
		present(db, "shorteventid_eventid", &event.to_be_bytes()),
	)
	.await?;

	Ok((key, event, !key_present, !event_present))
}

fn tally(census: Census, Scanned { id, parent, diff, absences }: Scanned) -> Result<Census> {
	let Some(diff) = diff else {
		return Ok(index(incomplete(census), id, parent, true));
	};

	let exposed = !diff.added.is_disjoint(&diff.removed);
	let seed = exposed || !absences.is_empty() || infected(&census.projection, &diff);
	let census = absences
		.into_iter()
		.fold(exposure(census, id, exposed), occurrence);

	Ok(index(census, id, parent, seed))
}

fn infected(projection: &Projection, diff: &Diff) -> bool {
	diff.added
		.iter()
		.chain(&diff.removed)
		.copied()
		.map(parse_compressed_state_event)
		.any(|(key, event)| {
			candidate(&projection.statekeys, key) || candidate(&projection.events, event)
		})
}

fn exposure(mut census: Census, id: Option<u64>, exposed: bool) -> Census {
	census.exposed.extend(id.filter(|_| exposed));
	census
}

fn candidate(mapping: &Mapping, short: u64) -> bool {
	mapping.aliases.contains_key(&short) || mapping.refused.contains(&short)
}

fn incomplete(census: Census) -> Census { Census { complete: false, ..census } }

fn occurrence(mut census: Census, (key, event, key_missing, event_missing): Absence) -> Census {
	if key_missing {
		census
			.orphans
			.entry(key)
			.or_default()
			.insert(event);

		census.projection.statekeys.refused.insert(key);
	}

	if event_missing {
		census.projection.events.refused.insert(event);
	}

	census
}

fn index(mut census: Census, id: Option<u64>, parent: Option<u64>, seed: bool) -> Census {
	census.unknown |= parent.is_none();
	let parent = parent.filter(|parent| *parent != 0);
	let Some(id) = id.filter(|id| *id != 0) else {
		census.held.extend(parent);
		return census;
	};

	census.parents.insert(id, parent);

	if seed {
		census.seeds.insert(id);
	}

	census
}

fn descendants(parents: &Parents, seeds: &BTreeSet<u64>) -> BTreeSet<u64> {
	parents
		.keys()
		.copied()
		.filter(|id| ancestors(parents, *id).any(|ancestor| seeds.contains(&ancestor)))
		.collect()
}

fn ancestors(parents: &Parents, id: u64) -> impl Iterator<Item = u64> + '_ {
	repeat(()).scan((Some(id), BTreeSet::new()), |(next, visited), ()| {
		let id = (*next)?;

		if !visited.insert(id) {
			return None;
		}

		*next = parents.get(&id).copied().flatten();
		Some(id)
	})
}

fn depth(parents: &Parents, id: u64) -> Option<usize> {
	let (depth, root) = ancestors(parents, id)
		.fold((0_usize, None), |(depth, _), id| (depth.saturating_add(1), Some(id)));

	parents
		.get(&root?)
		.is_some_and(Option::is_none)
		.then_some(depth)
}

fn block(mut blocked: BTreeSet<u64>, parents: &Parents, id: u64) -> BTreeSet<u64> {
	blocked.extend(ancestors(parents, id));
	blocked
}

#[tracing::instrument(level = "trace", skip_all)]
async fn digests(services: &Services, affected: &BTreeSet<u64>) -> Result<Claimed> {
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
		.ready_and_then(|row| services.server.check_running().map(|()| row))
		.inspect_ok(|_| services.server.progress.advance())
		.ready_try_fold(Claimed::new(), gather)
		.await
}

fn claim(value: &[u8]) -> Option<u64> { value.get(..8).and_then(short_of) }

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

#[tracing::instrument(level = "trace", skip_all)]
async fn materialize(services: &Services, id: u64) -> Result<Materialized> {
	let Some(original) = row(&services.db, id).await? else {
		return Ok(None);
	};

	let bytes = original.as_slice();
	let chain = unfold((Some(id), BTreeSet::new()), async |(next, visited)| {
		let next = next?;
		let loaded = match services.server.check_running() {
			| Err(error) => Err(error),
			| Ok(()) if visited.contains(&next) => Ok(None),
			| Ok(()) if next == id => Ok(decode(bytes)),
			| Ok(()) => layer(&services.db, next).await,
		};

		let parent = loaded
			.as_ref()
			.ok()
			.and_then(Option::as_ref)
			.and_then(|diff| diff.parent);

		Some((loaded, (parent, visit(visited, next))))
	})
	.try_collect::<Vec<_>>()
	.await?;

	let state = chain
		.into_iter()
		.rev()
		.try_fold(CompressedState::new(), |state, diff| {
			// Overlapping runs do not establish their historical state.
			diff.filter(|diff| diff.added.is_disjoint(&diff.removed))
				.map(|diff| apply(state, diff))
		});

	Ok(state.map(|state| (original, state)))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn layer(db: &Database, id: u64) -> Result<Option<Diff>> {
	fetch(db, "shortstatehash_statediff", &id.to_be_bytes(), decode)
		.map_ok(Option::flatten)
		.await
}

fn visit(mut visited: BTreeSet<u64>, id: u64) -> BTreeSet<u64> {
	visited.insert(id);
	visited
}

fn apply(state: CompressedState, diff: Diff) -> CompressedState {
	let Diff { parent, added, removed } = diff;

	// Extend the larger set, so a root layer is adopted rather than rebuilt.
	let (mut state, smaller) = match state.len() >= added.len() {
		| true => (state, added),
		| false => (added, state),
	};

	state.extend(smaller);

	if parent.is_some() {
		for entry in &removed {
			state.remove(entry);
		}
	}

	state
}

fn decode(bytes: &[u8]) -> Option<Diff> {
	let parent = short_of(bytes.get(..8)?)?;
	let parent = parent.ne(&0).then_some(parent);
	let tail = bytes.get(8..)?;
	let separator = (0..tail.len())
		.step_by(16)
		.find(|offset| tail.get(*offset..offset.saturating_add(8)) == Some(&[0; 8]));

	let (added, removed) = match separator {
		| None => (tail, &[][..]),
		| Some(offset) => {
			let removed = tail
				.get(offset.checked_add(8)?..)
				.filter(|removed| !removed.is_empty())?;

			(tail.get(..offset)?, removed)
		},
	};

	let added = entries(added)?;
	let removed = entries(removed)?;

	Some(Diff { parent, added, removed })
}

fn entries(bytes: &[u8]) -> Option<CompressedState> {
	let (chunks, []): (&[CompressedStateEvent], _) = bytes.as_chunks() else {
		return None;
	};

	chunks
		.iter()
		.copied()
		.map(|entry| {
			let (key, event) = parse_compressed_state_event(entry);
			let valid = key != 0 && event != 0;

			valid.then_some(entry)
		})
		.collect()
}

#[tracing::instrument(level = "trace", skip_all)]
async fn mapping(services: &Services, family: &Family) -> Result<Mapping> {
	let initial = Mapping {
		aliases: Aliases::new(),
		refused: BTreeSet::new(),
	};

	family
		.candidates
		.iter()
		.try_stream()
		.broad_and_then(async |candidate| {
			services.server.check_running()?;
			let winner = match candidate.kind {
				| Kind::Alias(winner) =>
					admitted(&services.db, family, candidate)
						.map_ok(|admitted| admitted.then_some(winner))
						.await?,
				| _ => None,
			};

			Ok((candidate, winner))
		})
		.ready_try_fold(initial, |mapping, (candidate, winner)| {
			Ok(classify(mapping, candidate, winner))
		})
		.await
}

fn classify(mut mapping: Mapping, candidate: &Candidate, winner: Option<u64>) -> Mapping {
	match winner {
		| None => {
			mapping.refused.insert(candidate.short);
		},
		| Some(winner) => {
			mapping
				.aliases
				.insert(candidate.short, (winner, candidate.identity.clone()));
		},
	}

	mapping
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

fn project(state: CompressedState, projection: &Projection) -> Option<CompressedState> {
	state
		.into_iter()
		.map(|entry| project_entry(entry, projection))
		.collect::<Option<CompressedState>>()
		.filter(unique_keys)
}

fn project_entry(
	entry: CompressedStateEvent,
	projection: &Projection,
) -> Option<CompressedStateEvent> {
	let (key, event) = parse_compressed_state_event(entry);
	let (key, _) = projection.statekeys.resolve(key)?;
	let (event, _) = projection.events.resolve(event)?;

	Some(compress_state_event(key, event))
}

#[implement(Mapping)]
fn resolve(&self, short: u64) -> Option<(u64, Option<&Identity>)> {
	self.refused.contains(&short).is_false().then(|| {
		self.aliases
			.get(&short)
			.map_or((short, None), |(winner, identity)| (*winner, Some(identity)))
	})
}

fn unique_keys(state: &CompressedState) -> bool {
	// Entries order by key half first, so a duplicate key sits beside its twin.
	state
		.iter()
		.map(|entry| parse_compressed_state_event(*entry).0)
		.tuple_windows()
		.all(|(left, right)| left != right)
}

#[tracing::instrument(level = "trace", skip_all)]
async fn identity(
	db: &Database,
	mapping: &Mapping,
	short: u64,
	reverse: &str,
) -> Result<Option<(u64, Identity)>> {
	let Some((winner, known)) = mapping.resolve(short) else {
		return Ok(None);
	};

	match known {
		| Some(identity) => Ok(Some((winner, identity.clone()))),
		| None =>
			lookup(db, reverse, &short.to_be_bytes())
				.map_ok(|identity| identity.map(|identity| (short, identity)))
				.await,
	}
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

#[tracing::instrument(level = "trace", skip_all)]
async fn row(db: &Database, id: u64) -> Result<Option<ValBuf>> {
	fetch(db, "shortstatehash_statediff", &id.to_be_bytes(), ValBuf::from_slice).await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn lookup(db: &Database, map: &str, key: &[u8]) -> Result<Option<Identity>> {
	fetch(db, map, key, Identity::from_slice).await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn fetch<T>(
	db: &Database,
	map: &str,
	key: &[u8],
	convert: impl FnOnce(&[u8]) -> T,
) -> Result<Option<T>> {
	db[map]
		.get(key)
		.map_ok(|value| convert(&value))
		.map(NotFound::present)
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn present(db: &Database, map: &str, key: &[u8]) -> Result<bool> {
	db[map]
		.exists(key)
		.map(NotFound::present)
		.map_ok(|found| found.is_some())
		.await
}

fn statekey(kind: &TimelineEventType, state_key: &str) -> Identity {
	kind.to_cow_str()
		.as_bytes()
		.iter()
		.copied()
		.chain([SEP])
		.chain(state_key.as_bytes().iter().copied())
		.collect()
}
