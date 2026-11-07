use std::{
	collections::{BTreeMap, BTreeSet},
	iter::repeat,
};

use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future::try_join, stream::unfold,
};
use ruma::events::TimelineEventType;
use tuwunel_core::{
	Err, Result, implement,
	itertools::Itertools,
	smallvec::SmallVec,
	utils::{
		BoolExt, IterStream, TryReadyExt,
		result::NotFound,
		stream::{TryBroadbandExt, TryWidebandExt},
	},
};
use tuwunel_database::{Database, SEP, keyval::ValBuf};

use super::{
	Identity, Reason, Shape,
	check::{Finding, Subject},
	identity::{Candidate, Family, Identities, Kind, admitted},
	short_of,
};
use crate::{
	Services,
	integrity::ScanExt,
	rooms::state_compressor::{
		CompressedState, CompressedStateEvent, compress_state_event, parse_compressed_state_event,
	},
};

pub(crate) mod history;
pub(crate) type Parents = BTreeMap<u64, Option<u64>>;
pub(crate) type Aliases = BTreeMap<u64, (u64, Identity)>;
type Occurrences = BTreeMap<u64, BTreeSet<u64>>;
pub(crate) type Absences = Vec<Absence>;
type Absence = (u64, u64, bool, bool);
type Observations = [Option<Observation>; 2];
type Observation = (Shape, u64, Reason);
pub(crate) type Materialized = Option<(ValBuf, CompressedState)>;
type Found = SmallVec<[Finding; 1]>;

pub(crate) struct Mapping {
	pub(crate) aliases: Aliases,
	pub(crate) refused: BTreeSet<u64>,
}

pub(crate) struct Projection {
	pub(crate) statekeys: Mapping,
	pub(crate) events: Mapping,
}

pub(crate) struct Census {
	pub(crate) parents: Parents,
	pub(crate) seeds: BTreeSet<u64>,
	pub(crate) unknown: bool,
	pub(crate) held: Vec<u64>,
	pub(crate) orphans: Occurrences,
	pub(crate) complete: bool,
	pub(crate) projection: Projection,
	pub(crate) exposed: BTreeSet<u64>,
}

pub(crate) struct Scanned {
	pub(crate) id: Option<u64>,
	pub(crate) parent: Option<u64>,
	pub(crate) diff: Option<Diff>,
	pub(crate) absences: Absences,
}

pub(crate) struct Diff {
	pub(crate) parent: Option<u64>,
	pub(crate) added: CompressedState,
	pub(crate) removed: CompressedState,
}

/// Streams what the state census finds, in the order verification records it.
///
/// A census that could not attribute every row leads, then each affected state
/// follows with its own findings, inspected one at a time.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) fn findings(
	services: &Services,
	census: Census,
) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	let incomplete = census.unknown || !census.complete || !census.held.is_empty();
	let head = incomplete.then_some(Finding {
		shape: Shape::AffectedState,
		subject: Subject::Incomplete,
		reason: Reason::References,
	});

	let affected = descendants(&census.parents, &census.seeds).into_iter();

	// Serial: each state materializes in full, and its entries already fan out.
	// A state's findings are owned, as no item may borrow the census the unfold holds.
	let states = unfold((census, affected), async |(census, mut affected)| {
		let id = affected.next()?;
		let found = inspect_state(services, &census, id).await;

		Some((found, (census, affected)))
	});

	head.try_stream().chain(
		states
			.map_ok(IterStream::try_stream)
			.try_flatten(),
	)
}

#[tracing::instrument(level = "trace", skip_all)]
async fn inspect_state(services: &Services, census: &Census, id: u64) -> Result<Found> {
	services.server.check_running()?;
	let state = |shape| Finding {
		shape,
		subject: Subject::State(id),
		reason: Reason::HistoricalState,
	};

	let Some(bytes) = row(&services.db, id).await? else {
		return Err!("state disappeared during final injectivity verification");
	};

	let Some(diff) = decode(&bytes) else {
		return Ok([state(Shape::AffectedState)].into());
	};

	let collided = diff
		.added
		.is_disjoint(&diff.removed)
		.is_false()
		.then(|| state(Shape::DiffCollision));

	let found = diff
		.added
		.iter()
		.chain(&diff.removed)
		.copied()
		.try_stream()
		.wide_and_then(|entry| inspect_entry(services, &census.projection, entry))
		.ready_try_fold(collided.into_iter().collect(), observed)
		.await?;

	let invalid = materialize(services, id)
		.await?
		.and_then(|(_, state)| project(state, &census.projection))
		.is_none();

	let unverified = invalid || !census.seeds.contains(&id);

	Ok(extended(found, unverified.then(|| state(Shape::AffectedState))))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn inspect_entry(
	services: &Services,
	projection: &Projection,
	entry: CompressedStateEvent,
) -> Result<Observations> {
	let (key, event) = parse_compressed_state_event(entry);
	let key = inspect_half(
		services,
		&projection.statekeys,
		key,
		"shortstatekey_statekey",
		(Shape::StatekeyAliasEntry, Shape::StatekeyOrphanEntry),
	);

	let event = inspect_half(
		services,
		&projection.events,
		event,
		"shorteventid_eventid",
		(Shape::EventAliasEntry, Shape::EventOrphanEntry),
	);

	try_join(key, event).map_ok(Into::into).await
}

fn observed(found: Found, observations: Observations) -> Result<Found> {
	let entries = observations
		.into_iter()
		.flatten()
		.map(|(shape, short, reason)| Finding {
			shape,
			subject: Subject::Short(short),
			reason,
		});

	Ok(extended(found, entries))
}

fn extended(mut found: Found, more: impl IntoIterator<Item = Finding>) -> Found {
	found.extend(more);
	found
}

#[tracing::instrument(level = "trace", skip_all)]
async fn inspect_half(
	services: &Services,
	mapping: &Mapping,
	short: u64,
	reverse: &str,
	(alias, orphan): (Shape, Shape),
) -> Result<Option<Observation>> {
	if !present(&services.db, reverse, &short.to_be_bytes()).await? {
		return Ok(Some((orphan, short, Reason::Identity)));
	}

	Ok(candidate(mapping, short).then_some((alias, short, Reason::HistoricalState)))
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn survey(services: &Services, identities: &Identities) -> Result<Census> {
	let (statekeys, events) =
		try_join(mapping(services, &identities.statekeys), mapping(services, &identities.events))
			.await?;

	census(services, Projection { statekeys, events }).await
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn census(services: &Services, projection: Projection) -> Result<Census> {
	let statekeys = presence(services, "shortstatekey_statekey").await?;
	let events = presence(services, "shorteventid_eventid").await?;
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
		.scanned(&services.server)
		.map_ok(|(key, value)| Scanned {
			id: short_of(key),
			parent: value.get(..8).and_then(short_of),
			diff: decode(value),
			absences: Absences::new(),
		})
		.ready_try_fold(initial, |census, scanned| {
			tally(census, scan(&statekeys, &events, scanned))
		})
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn presence(services: &Services, column: &str) -> Result<Vec<u64>> {
	// Exact big-endian keys retain numeric order, regardless of their values' validity.
	services.db[column]
		.raw_keys()
		.scanned(&services.server)
		.ready_try_filter_map(|key| Ok(short_of(key)))
		.try_collect()
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn scan(statekeys: &[u64], events: &[u64], scanned: Scanned) -> Scanned {
	let Some(diff) = scanned.diff.as_ref() else {
		return scanned;
	};

	let absences = diff
		.added
		.iter()
		.chain(&diff.removed)
		.copied()
		.map(|entry| absence(statekeys, events, entry))
		.filter(|(_, _, key_missing, event_missing)| *key_missing || *event_missing)
		.collect();

	Scanned { absences, ..scanned }
}

#[tracing::instrument(level = "trace", skip_all)]
fn absence(statekeys: &[u64], events: &[u64], entry: CompressedStateEvent) -> Absence {
	let (key, event) = parse_compressed_state_event(entry);
	let key_missing = statekeys.binary_search(&key).is_err();
	let event_missing = events.binary_search(&event).is_err();

	(key, event, key_missing, event_missing)
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

pub(crate) fn index(
	mut census: Census,
	id: Option<u64>,
	parent: Option<u64>,
	seed: bool,
) -> Census {
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

pub(crate) fn descendants(parents: &Parents, seeds: &BTreeSet<u64>) -> BTreeSet<u64> {
	if seeds.is_empty() {
		return BTreeSet::new();
	}

	parents
		.keys()
		.copied()
		.filter(|id| ancestors(parents, *id).any(|ancestor| seeds.contains(&ancestor)))
		.collect()
}

pub(crate) fn ancestors(parents: &Parents, id: u64) -> impl Iterator<Item = u64> + '_ {
	repeat(()).scan((Some(id), BTreeSet::new()), |(next, visited), ()| {
		let id = (*next)?;

		if !visited.insert(id) {
			return None;
		}

		*next = parents.get(&id).copied().flatten();
		Some(id)
	})
}

pub(crate) fn depth(parents: &Parents, id: u64) -> Option<usize> {
	let (depth, root) = ancestors(parents, id)
		.fold((0_usize, None), |(depth, _), id| (depth.saturating_add(1), Some(id)));

	parents
		.get(&root?)
		.is_some_and(Option::is_none)
		.then_some(depth)
}

pub(crate) fn claim(value: &[u8]) -> Option<u64> { value.get(..8).and_then(short_of) }

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn materialize(services: &Services, id: u64) -> Result<Materialized> {
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
pub(crate) async fn layer(db: &Database, id: u64) -> Result<Option<Diff>> {
	fetch(db, "shortstatehash_statediff", &id.to_be_bytes(), decode)
		.map_ok(Option::flatten)
		.await
}

pub(crate) fn visit(mut visited: BTreeSet<u64>, id: u64) -> BTreeSet<u64> {
	visited.insert(id);
	visited
}

pub(crate) fn apply(state: CompressedState, diff: Diff) -> CompressedState {
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

pub(crate) fn decode(bytes: &[u8]) -> Option<Diff> {
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
pub(crate) async fn mapping(services: &Services, family: &Family) -> Result<Mapping> {
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

pub(crate) fn project(
	state: CompressedState,
	projection: &Projection,
) -> Option<CompressedState> {
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
pub(crate) async fn identity(
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
pub(crate) async fn row(db: &Database, id: u64) -> Result<Option<ValBuf>> {
	fetch(db, "shortstatehash_statediff", &id.to_be_bytes(), ValBuf::from_slice).await
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn lookup(db: &Database, map: &str, key: &[u8]) -> Result<Option<Identity>> {
	fetch(db, map, key, Identity::from_slice).await
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn fetch<T>(
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
pub(crate) async fn present(db: &Database, map: &str, key: &[u8]) -> Result<bool> {
	db[map]
		.exists(key)
		.map(NotFound::present)
		.map_ok(|found| found.is_some())
		.await
}

pub(crate) fn statekey(kind: &TimelineEventType, state_key: &str) -> Identity {
	kind.to_cow_str()
		.as_bytes()
		.iter()
		.copied()
		.chain([SEP])
		.chain(state_key.as_bytes().iter().copied())
		.collect()
}
