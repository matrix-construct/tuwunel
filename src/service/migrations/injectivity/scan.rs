use std::{
	cmp::Ordering,
	collections::{BTreeMap, BTreeSet},
	sync::Arc,
};

use futures::StreamExt;
use serde::Deserialize;
use tuwunel_core::{
	Progress, Result,
	arrayvec::ArrayVec,
	err, implement, info,
	smallvec::SmallVec,
	utils::{
		BoolExt, ReadyExt, TryReadyExt,
		stream::{BroadbandExt, IterStream, TryIgnore},
	},
	warn,
};
use tuwunel_database::{Database, Get, Handle, Map, SEP};

use super::Reason;
use crate::{
	Services,
	rooms::{pdu_metadata::typed_relations::Key as RelationKey, state_compressor::StateDiff},
};

/// Owned copy of a reverse-map identity, used to dereference a loser.
///
/// Sized for the common modern event id; longer identities spill.
type Identity = SmallVec<[u8; 48]>;

/// The `relatesto_typed` rows to rewrite, each with its stale child value.
///
/// The key is copied verbatim and rewritten at its own row; one wider than
/// the writer's fixed length cannot be a relation row and is skipped.
pub(super) type Relations = Vec<(RelationKey, u64)>;

/// Bitmap over the short id space, one bit per id up to the global counter.
///
/// Out-of-range bits are silently absent: setting one is a no-op and
/// testing one is false.
type Bits = Vec<u64>;

/// One short id paired with the identity its row names.
type Candidate = (u64, Identity);

/// Reverse rows no forward value claims, paired with the identities they
/// name.
type Candidates = Vec<Candidate>;

/// One family's resolution: losers, the winner each maps to, the rows a
/// promotion can heal, and the count the dereference could not settle.
///
/// Only a row proved absent is promotable; an unsettled one may still hold
/// a live forward row, so it refuses instead.
type Resolution = (Vec<u64>, BTreeMap<u64, u64>, Candidates, u64);

/// What dereferencing one candidate's identity proved.
///
/// Absent is the only outcome a promotion may act on. A forward row whose
/// value is not a short id remains unsettled, which is not the same as
/// proving nothing is there; a failed read never produces an outcome.
enum Resolved {
	Winner(u64),
	Absent,
	Unsettled,
}

/// Exclusive upper bound on verifiable short ids.
///
/// Each scan bitmap costs one bit per id up to the global counter, and the
/// deep sweep holds up to four at once. At or above this bound the scan
/// reports unverifiable instead.
const MAX_SHORT: u64 = 1 << 30;

/// Format word leading every decline record.
///
/// A reader finding any other value here must treat the remaining words
/// as opaque rather than assume this layout.
const RECORD_FORMAT: u64 = 1;

/// The counters a declined repair stamps as the marker value.
///
/// Twenty-two words: the format, the reason, the global counter, seven
/// counts per family (rows, losers, dangling, promotable, contended,
/// unresolved, malformed) for events then statekeys, and the five deep
/// counts (infected, infected parents, orphans, malformed diffs,
/// colliding diffs). The reason alone decides which words were measured:
/// an
/// unverifiable scan measured nothing past the counter, a healable or
/// family-anomalous verdict measured the families only, and only a deep
/// decline measured everything, so an unmeasured zero is no measurement.
/// A nonzero malformed-diffs word also voids the colliding word, whose
/// census skips impeached framing. The value serializer packs the words
/// as contiguous big-endian bytes.
pub(super) type DeclineRecord = ArrayVec<u64, 22>;

/// One family's residue: its losers, their winners, the rows a heal pass
/// completes, and the counts that impugn the scan.
///
/// The losers include every row the dereference could not pair, so
/// `winners` is total exactly when `promotable` is empty and `unresolved`
/// is zero. A contended slot leaves both its claimants unhealed, since
/// nothing in the residue names which one the allocator meant.
#[derive(Default)]
pub(super) struct Family {
	pub(super) rows: u64,
	pub(super) losers: Vec<u64>,
	pub(super) winners: BTreeMap<u64, u64>,
	pub(super) dangling: Candidates,
	pub(super) promotable: Candidates,
	pub(super) contended: u64,
	pub(super) unresolved: u64,
	pub(super) malformed: u64,
}

/// Everything one scan measured, and the worklists the repair consumes.
///
/// The deep counts stay zero unless the sweep ran: a loserless scan, a
/// healable residue, and a family anomaly all decide the boot without
/// reading the deeper indexes.
#[derive(Default)]
pub(super) struct Scan {
	pub(super) counter: u64,
	pub(super) events: Family,
	pub(super) statekeys: Family,
	pub(super) dirty: u64,
	pub(super) entries: u64,
	pub(super) infected: BTreeSet<u64>,
	pub(super) infected_parents: u64,
	pub(super) orphans: u64,
	pub(super) malformed_diffs: u64,
	pub(super) colliding_diffs: u64,
	pub(super) moves: Vec<u64>,
	pub(super) relations: Relations,
	pub(super) strays: u64,
	pub(super) unverifiable: bool,
}

/// Statediff walk context: the bitmaps each row's entries are tested
/// against.
///
/// Folded with a [`Counts`] accumulator over every
/// `shortstatehash_statediff` row by [`Diffs::row`].
struct Diffs<'a> {
	counter: u64,
	event_stale: &'a [u64],
	statekey_stale: &'a [u64],
	event_reverse: &'a [u64],
	statekey_reverse: &'a [u64],
}

/// Counts the statediff walk accumulates.
///
/// `malformed` covers rows the framing rejects, entries included; the
/// ghost tallies surface in the sweep's log and the remaining fields
/// mirror their [`Scan`] counterparts.
#[derive(Default)]
struct Counts {
	infected: BTreeSet<u64>,
	infected_parents: u64,
	ghosts: u64,
	removed_ghosts: u64,
	orphans: u64,
	malformed: u64,
}

/// The `sroomid` field of a stored notification value.
///
/// A mirror of the pusher's stored shape just wide enough for the stray
/// census; every other field is ignored.
#[derive(Deserialize)]
struct Notification {
	sroomid: u64,
}

/// Measures short id injectivity across both families.
///
/// Each reverse map streams before its forward map, so a concurrent
/// allocation surfaces only on the forward side and cannot be flagged
/// stale. The deeper indexes are read only when a loser exists and no
/// family-level verdict already decided the boot.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn scan(services: &Services) -> Result<Scan> {
	info!("Scanning ShortID columns for duplicate values...");

	let counter = services.globals.current_count();

	if counter >= MAX_SHORT {
		warn!(
			%counter,
			"Short id space too large to verify injectivity; refusing the destructive repair."
		);
		return Ok(Scan {
			counter,
			unverifiable: true,
			..Default::default()
		});
	}

	let words = usize::try_from((counter / 64).saturating_add(1))
		.map_err(|_| err!("short id bitmap exceeds the address width"))?;

	let progress = &services.server.progress;

	progress.begin("fix_short_injectivity: event short ids");
	let (events, event_reverse) =
		family(services, "eventid_shorteventid", "shorteventid_eventid", counter, words).await?;

	progress.begin("fix_short_injectivity: state key short ids");
	let (statekeys, statekey_reverse) =
		family(services, "statekey_shortstatekey", "shortstatekey_statekey", counter, words)
			.await?;

	if events.losers.is_empty() && statekeys.losers.is_empty() {
		return Ok(Scan {
			counter,
			events,
			statekeys,
			..Default::default()
		});
	}

	let families = Scan {
		counter,
		events,
		statekeys,
		..Default::default()
	};

	// Both a heal and a family anomaly decide the boot without a deep
	// count, so neither pays for one.
	if families.family_anomalous() || families.healable() {
		return Ok(families);
	}

	let Scan { events, statekeys, .. } = families;

	progress.begin("fix_short_injectivity: deep indexes");
	let swept =
		sweep(services, &events, &statekeys, event_reverse, statekey_reverse, counter).await?;

	progress.begin("fix_short_injectivity: colliding state diffs");

	// Malformed framing already refuses the lane, and the typed read would
	// trust the framing the walk just impeached.
	let colliding_diffs = match swept.malformed_diffs {
		| 0 => colliding_diffs(services, &swept.infected).await?,
		| _ => 0,
	};

	Ok(Scan {
		counter,
		events,
		statekeys,
		colliding_diffs,
		..swept
	})
}

/// Whether any count impugns the repair's own input.
///
/// Any anomaly refuses the destructive repair lane; the cache-clearing
/// lane is unconditionally safe and proceeds regardless. A malformed diff
/// row leaves the statediff walk's own output in doubt, a child of an
/// infected state would need its whole chain rederived, and an infected
/// row whose stored runs already intersect is malformed input no writer
/// produces, so all three gate alongside the family anomalies.
#[implement(Scan)]
pub(super) fn anomalous(&self) -> bool {
	self.family_anomalous()
		|| self.infected_parents > 0
		|| self.malformed_diffs > 0
		|| self.colliding_diffs > 0
}

/// Whether either short id family carries an unhandled shape.
///
/// This predicate uses only the family passes, so callers can distinguish
/// their early verdict from counters a deep sweep never measured.
#[implement(Scan)]
pub(super) fn family_anomalous(&self) -> bool {
	self.events.anomalous() || self.statekeys.anomalous()
}

/// Whether a heal pass has rows to complete in either family.
///
/// A healable scan decides nothing else until it rescans, since a heal
/// changes the losers and winners every deeper measure is taken against.
#[implement(Scan)]
pub(super) fn healable(&self) -> bool { self.events.healable() || self.statekeys.healable() }

/// Packs the verdict's counters into the value a decline stamps.
///
/// The record outlives the boot log, handing a follow-up migration and
/// any support thread the counters as they stood before the stamp.
/// [`DeclineRecord`] owns the layout; a length past the integer range
/// saturates rather than wrapping.
#[implement(Scan)]
pub(super) fn decline_record(&self, reason: Reason) -> DeclineRecord {
	let count = |len: usize| u64::try_from(len).unwrap_or(u64::MAX);

	let record: [u64; 22] = [
		RECORD_FORMAT,
		reason.into(),
		self.counter,
		self.events.rows,
		count(self.events.losers.len()),
		count(self.events.dangling.len()),
		count(self.events.promotable.len()),
		self.events.contended,
		self.events.unresolved,
		self.events.malformed,
		self.statekeys.rows,
		count(self.statekeys.losers.len()),
		count(self.statekeys.dangling.len()),
		count(self.statekeys.promotable.len()),
		self.statekeys.contended,
		self.statekeys.unresolved,
		self.statekeys.malformed,
		count(self.infected.len()),
		self.infected_parents,
		self.orphans,
		self.malformed_diffs,
		self.colliding_diffs,
	];

	record.into()
}

/// Whether this family carries a shape the repair does not handle.
///
/// A contended slot has two claimants and nothing to break the tie, an
/// unresolved row was never proved absent, and a malformed key leaves the
/// bitmaps too incomplete for any other verdict to stand.
#[implement(Family)]
fn anomalous(&self) -> bool { self.contended > 0 || self.unresolved > 0 || self.malformed > 0 }

/// Whether this family has rows a heal pass completes.
///
/// Anything impugning the family withholds both classes: the same
/// bitmaps that name a dangling winner are the ones a malformed key
/// leaves incomplete.
#[implement(Family)]
pub(super) fn healable(&self) -> bool {
	!self.anomalous() && (!self.dangling.is_empty() || !self.promotable.is_empty())
}

/// Scans one family in two passes, and a third only where the bitmaps
/// disagree.
///
/// The reverse bitmap completes before the forward stream begins, so a
/// concurrent allocation surfaces only forward-side and cannot be counted
/// dangling or stale. The third pass names each loser and its identity; on
/// a clean family the bitmap difference proves there are none, so it never
/// runs. Returns the family and its reverse-key bitmap, which the deep sweep
/// reuses to detect orphaned statediff entries.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%forward,
		%reverse,
	),
)]
async fn family(
	services: &Services,
	forward: &'static str,
	reverse: &'static str,
	counter: u64,
	words: usize,
) -> Result<(Family, Bits)> {
	let db = &services.db;
	let progress = &services.server.progress;

	progress.enter("reverse rows");
	let (reverse_bits, rows, reverse_malformed) =
		reverse_bitmap(&db[reverse], words, progress).await?;

	progress.enter("forward rows");
	let (forward_bits, mut dangling, forward_malformed) =
		dangling_winners(&db[forward], &reverse_bits, counter, words, progress).await?;

	progress.enter("unclaimed reverse rows");

	// A set reverse bit no forward value claims is what the pass collects.
	let candidates = match any_unclaimed(&reverse_bits, &forward_bits, counter) {
		| false => Candidates::new(),
		| true => loser_candidates(&db[reverse], &forward_bits, counter, progress).await?,
	};

	drop(forward_bits);

	progress.enter("candidate identities");
	let (losers, winners, mut promotable, unresolved) =
		resolve(&db[forward], &candidates, progress).await?;

	let contended = contenders(&mut dangling, by_short)
		.saturating_add(contenders(&mut promotable, by_identity));

	let family = Family {
		rows,
		losers,
		winners,
		dangling,
		promotable,
		contended,
		unresolved,
		malformed: reverse_malformed.saturating_add(forward_malformed),
	};

	info!(
		%forward,
		%reverse,
		rows = family.rows,
		losers = family.losers.len(),
		dangling = family.dangling.len(),
		promotable = family.promotable.len(),
		contended = family.contended,
		unresolved = family.unresolved,
		malformed = family.malformed,
		"Finished scanning column pair."
	);

	Ok((family, reverse_bits))
}

/// Streams a reverse map into its keyset bitmap, its row count, and its
/// count of keys that are not an 8-byte short id.
///
/// The rows are counted here rather than in the loser pass, which a clean
/// family skips.
async fn reverse_bitmap(
	map: &Arc<Map>,
	words: usize,
	progress: &Progress,
) -> Result<(Bits, u64, u64)> {
	map.raw_keys()
		.ready_try_fold((vec![0_u64; words], 0_u64, 0_u64), |(mut bits, rows, malformed), key| {
			progress.advance();

			let rows = rows.saturating_add(1);

			Ok(match short_of(key) {
				| None => (bits, rows, malformed.saturating_add(1)),
				| Some(short) => {
					set_bit(&mut bits, short);

					(bits, rows, malformed)
				},
			})
		})
		.await
}

/// Streams a forward map against the reverse bitmap for dangling winners.
///
/// A dangling winner is a forward value no reverse row answers for.
/// Values past the counter are concurrent allocations, not danglings. Each
/// one carries the identity its forward row is keyed by, which is the
/// reverse row a heal reinstates.
async fn dangling_winners(
	map: &Arc<Map>,
	reverse_bits: &[u64],
	counter: u64,
	words: usize,
	progress: &Progress,
) -> Result<(Bits, Candidates, u64)> {
	map.raw_stream()
		.ready_try_fold(
			(vec![0_u64; words], Candidates::new(), 0_u64),
			|(mut bits, mut dangling, malformed), (key, value)| {
				progress.advance();

				Ok(match short_of(value) {
					| None => (bits, dangling, malformed.saturating_add(1)),
					| Some(short) => {
						if short <= counter && !get_bit(reverse_bits, short) {
							dangling.push((short, Identity::from_slice(key)));
						}

						set_bit(&mut bits, short);

						(bits, dangling, malformed)
					},
				})
			},
		)
		.await
}

/// Whether any reverse key's short id went unclaimed by a forward value.
///
/// The bitmaps round up to a whole word, so ids past the counter are
/// addressable in the last one and are masked off. The mask keeps the
/// counter's own bit, matching the bound the loser pass applies.
fn any_unclaimed(reverse_bits: &[u64], forward_bits: &[u64], counter: u64) -> bool {
	let last = usize::try_from(counter / 64).unwrap_or(usize::MAX);
	let tail = u64::MAX >> 63_u64.saturating_sub(counter % 64);

	debug_assert_eq!(reverse_bits.len(), last.saturating_add(1), "bitmap spans the counter");
	debug_assert_eq!(forward_bits.len(), reverse_bits.len(), "bitmaps span one id space");

	reverse_bits
		.iter()
		.copied()
		.zip(forward_bits.iter().copied())
		.enumerate()
		.any(|(word, (reverse, forward))| {
			let mask = match word < last {
				| true => u64::MAX,
				| false => tail,
			};

			(reverse & !forward & mask) != 0
		})
}

/// Collects reverse keys no forward value claims.
///
/// The identity each row names rides along for the dereference pass.
async fn loser_candidates(
	map: &Arc<Map>,
	forward_bits: &[u64],
	counter: u64,
	progress: &Progress,
) -> Result<Candidates> {
	map.raw_stream()
		.ready_try_fold(Candidates::new(), |mut candidates, (key, value)| {
			progress.advance();

			let unclaimed =
				short_of(key).filter(|short| *short <= counter && !get_bit(forward_bits, *short));

			if let Some(short) = unclaimed {
				candidates.push((short, Identity::from_slice(value)));
			}

			Ok(candidates)
		})
		.await
}

/// Dereferences each candidate's identity to split losers from winners.
///
/// The identity a loser's reverse row names must hold a live forward row,
/// whose value is the winner. A candidate resolving to itself was a
/// concurrent allocation, not a loser.
async fn resolve(
	map: &Arc<Map>,
	candidates: &[(u64, Identity)],
	progress: &Progress,
) -> Result<Resolution> {
	let (mut losers, winners, promotable, unsettled, paired) = candidates
		.iter()
		.map(candidate_identity)
		.stream()
		.get(map)
		.zip(candidates.iter().stream())
		.map(|(result, candidate)| resolution(result).map(|resolved| (resolved, candidate)))
		.ready_try_fold(
			(Vec::new(), BTreeMap::new(), Candidates::new(), 0_u64, 0_usize),
			|(mut losers, mut winners, mut promotable, unsettled, paired),
			 (resolved, candidate)| {
				progress.advance();

				let paired = paired.saturating_add(1);
				let loser = candidate_short(candidate);

				Ok(match resolved {
					| Resolved::Winner(winner) if winner == loser =>
						(losers, winners, promotable, unsettled, paired),
					| Resolved::Winner(winner) => {
						losers.push(loser);
						winners.insert(loser, winner);

						(losers, winners, promotable, unsettled, paired)
					},
					| Resolved::Absent => {
						losers.push(loser);
						promotable.push(candidate.clone());

						(losers, winners, promotable, unsettled, paired)
					},
					| Resolved::Unsettled => {
						losers.push(loser);

						(losers, winners, promotable, unsettled.saturating_add(1), paired)
					},
				})
			},
		)
		.await?;

	// A successful batch preserves cardinality. Retain a fail-closed tail
	// defense in case an adapter ever truncates a clean lookup stream.
	let tail = candidates.get(paired..).unwrap_or_default();
	losers.extend(tail.iter().map(candidate_short));

	let unresolved = unsettled.saturating_add(u64::try_from(tail.len()).unwrap_or(u64::MAX));

	Ok((losers, winners, promotable, unresolved))
}

/// Counts candidates contending for a slot another candidate already
/// claims.
///
/// Sorting is what makes contenders adjacent; the comparator names the
/// half of the pair that decides the slot, the short id for a
/// reinstatement and the identity for a promotion.
fn contenders<F>(candidates: &mut [Candidate], cmp: F) -> u64
where
	F: Fn(&Candidate, &Candidate) -> Ordering,
{
	candidates.sort_unstable_by(&cmp);

	let contenders = candidates
		.windows(2)
		.filter(|pair| cmp(&pair[0], &pair[1]).is_eq())
		.count();

	u64::try_from(contenders).unwrap_or(u64::MAX)
}

// Named for the higher-ranked closure generality the dereference stream
// needs; an inline closure pins the item lifetimes.
fn candidate_identity((_, identity): &Candidate) -> &Identity { identity }

fn candidate_short((short, _): &Candidate) -> u64 { *short }

fn by_short(a: &Candidate, b: &Candidate) -> Ordering { a.0.cmp(&b.0) }

fn by_identity(a: &Candidate, b: &Candidate) -> Ordering { a.1.cmp(&b.1) }

// A failed read is not an absent row; only the not-found error proves the
// forward row is missing, and a promotion is a write.
fn resolution(result: Result<Handle<'_>>) -> Result<Resolved> {
	match result {
		| Ok(handle) => Ok(short_of(&handle).map_or(Resolved::Unsettled, Resolved::Winner)),
		| Err(error) if error.is_not_found() => Ok(Resolved::Absent),
		| Err(error) => Err(error),
	}
}

/// Reads the deeper indexes for the worklists the repair consumes.
///
/// Statediff entries are tested against both families and chain-cache
/// rows against either. The caller runs this only when a loser exists and
/// no family-level verdict already decided the boot.
#[tracing::instrument(level = "debug", skip_all)]
async fn sweep(
	services: &Services,
	events: &Family,
	statekeys: &Family,
	event_reverse: Bits,
	statekey_reverse: Bits,
	counter: u64,
) -> Result<Scan> {
	let db = &services.db;
	let progress = &services.server.progress;
	let words = event_reverse.len();
	let event_stale = bits_of(&events.losers, words);
	let statekey_stale = bits_of(&statekeys.losers, words);

	let walk = Diffs {
		counter,
		event_stale: &event_stale,
		statekey_stale: &statekey_stale,
		event_reverse: &event_reverse,
		statekey_reverse: &statekey_reverse,
	};

	progress.enter("state diff rows");
	let counts = db["shortstatehash_statediff"]
		.raw_stream()
		.ready_try_fold(Counts::default(), |counts, (key, value)| {
			progress.advance();

			Ok(walk.row(counts, key, value))
		})
		.await?;

	drop(event_reverse);
	drop(statekey_reverse);

	// A key or value that is stale or not a whole number of short ids
	// poisons the row either way.
	progress.enter("auth chain rows");
	let (dirty, entries) = db["authchainkey_authchain"]
		.raw_stream()
		.ready_try_fold((0_u64, 0_u64), |(dirty, entries), (key, chain)| {
			progress.advance();

			let hit = disposable(key, &event_stale, &statekey_stale)
				|| disposable(chain, &event_stale, &statekey_stale);

			Ok((dirty.saturating_add(u64::from(hit)), entries.saturating_add(1)))
		})
		.await?;

	// ready_try_fold rather than ready_try_filter_map: the higher-ranked
	// adapter fails the boot coroutine's Send obligation over cursor-borrowed
	// items.
	progress.enter("event state rows");
	let moves: Vec<u64> = db["shorteventid_shortstatehash"]
		.raw_keys()
		.ready_try_fold(Vec::new(), |mut moves, key| {
			progress.advance();

			if let Some(loser) = short_of(key).filter(|short| get_bit(&event_stale, *short)) {
				moves.push(loser);
			}

			Ok(moves)
		})
		.await?;

	progress.enter("typed relation rows");
	let relations: Relations = db["relatesto_typed"]
		.raw_stream()
		.ready_try_fold(Relations::new(), |mut relations, (key, value)| {
			progress.advance();

			let dirty = short_of(value)
				.filter(|loser| get_bit(&event_stale, *loser))
				.zip(RelationKey::try_from(key).ok());

			if let Some((loser, key)) = dirty {
				relations.push((key, loser));
			}

			Ok(relations)
		})
		.await?;

	// dirty and entries read zero on a boot whose one-time chain clear ran
	// first; split-marker upgrades report the live dirt instead.
	warn!(
		dirty,
		entries,
		infected = counts.infected.len(),
		infected_parents = counts.infected_parents,
		ghosts = counts.ghosts,
		removed_ghosts = counts.removed_ghosts,
		orphans = counts.orphans,
		malformed_diffs = counts.malformed,
		moves = moves.len(),
		relations = relations.len(),
		"Swept the deeper short id indexes."
	);

	Ok(Scan {
		dirty,
		entries,
		infected: counts.infected,
		infected_parents: counts.infected_parents,
		orphans: counts.orphans,
		malformed_diffs: counts.malformed,
		moves,
		relations,
		..Default::default()
	})
}

impl Diffs<'_> {
	/// Folds one statediff row through the walk.
	///
	/// The value carries an 8-byte parent, then 16-byte entries of a
	/// statekey and an event half, an added run first and a removed run
	/// only behind an 8-byte zero sentinel. The sentinel shifts entry
	/// alignment by 8, so the walk is sequential rather than chunked.
	fn row(&self, mut counts: Counts, key: &[u8], value: &[u8]) -> Counts {
		let (Some(row), Some(parent)) = (short_of(key), value.get(0..8).and_then(short_of))
		else {
			counts.malformed = counts.malformed.saturating_add(1);
			return counts;
		};

		// Rows stream in ascending shortstatehash order, and the writer can
		// only name an already allocated parent. Failing this premise closed
		// keeps the one-pass descendant check sound.
		if parent != 0 && parent >= row {
			counts.malformed = counts.malformed.saturating_add(1);
			return counts;
		}

		if parent != 0 && counts.infected.contains(&parent) {
			counts.infected_parents = counts.infected_parents.saturating_add(1);
		}

		let mut removed_run = false;
		let mut removed = 0_u64;
		let mut at = 8_usize;

		while at < value.len() {
			if !removed_run && value[at..].starts_with(&0_u64.to_be_bytes()) {
				removed_run = true;
				at = at.saturating_add(8);
				continue;
			}

			let entries = (
				value
					.get(at..at.saturating_add(8))
					.and_then(short_of),
				value
					.get(at.saturating_add(8)..at.saturating_add(16))
					.and_then(short_of),
			);

			let (Some(statekey), Some(event)) = entries else {
				counts.malformed = counts.malformed.saturating_add(1);
				return counts;
			};

			removed = removed.saturating_add(u64::from(removed_run));

			if get_bit(self.statekey_stale, statekey) || get_bit(self.event_stale, event) {
				counts.infected.insert(row);
				counts.ghosts = counts.ghosts.saturating_add(1);
				counts.removed_ghosts = counts
					.removed_ghosts
					.saturating_add(u64::from(removed_run));
			}

			let orphaned = (statekey <= self.counter
				&& !get_bit(self.statekey_reverse, statekey))
				|| (event <= self.counter && !get_bit(self.event_reverse, event));

			counts.orphans = counts.orphans.saturating_add(u64::from(orphaned));
			at = at.saturating_add(16);
		}

		// The writer gates the sentinel on a nonempty removed run.
		if removed_run && removed == 0 {
			counts.malformed = counts.malformed.saturating_add(1);
		}

		counts
	}
}

/// Counts infected rows whose stored runs already intersect.
///
/// No writer emits a row whose added and removed runs share an entry, so
/// a hit is malformed input of unknown provenance and refuses the
/// destructive lane rather than being rewritten. The population is the
/// repair's own worklist, read back through the same typed round-trip the
/// patch trusts.
#[tracing::instrument(level = "debug", skip_all)]
async fn colliding_diffs(services: &Services, infected: &BTreeSet<u64>) -> Result<u64> {
	infected
		.iter()
		.copied()
		.stream()
		.broad_then(async |state| {
			services
				.state_compressor
				.get_statediff(state)
				.await
		})
		.ready_try_fold(0_u64, |colliding, diff| {
			Ok(colliding.saturating_add(u64::from(intersecting(&diff))))
		})
		.await
}

/// Whether one diff row's stored runs share an entry.
fn intersecting(diff: &StateDiff) -> bool { !diff.added.is_disjoint(&diff.removed) }

/// Counts shortroomid references with no forward row.
///
/// Purged rooms and losing allocations both produce them; no repair step
/// touches a shortroomid family, so the count reports and gates nothing.
/// Retained (unused) as the only in-tree measurement of the stray class,
/// one restored call away from a repair that learns to act on it.
#[tracing::instrument(level = "debug", skip_all)]
#[expect(dead_code)]
async fn strays(db: &Database, counter: u64, words: usize) -> u64 {
	let rooms = db["roomid_shortroomid"]
		.raw_stream()
		.ignore_err()
		.ready_fold(vec![0_u64; words], |mut bits, (_, value)| {
			if let Some(short) = short_of(value) {
				set_bit(&mut bits, short);
			}

			bits
		})
		.await;

	let stray = |short: Option<u64>| {
		u64::from(short.is_some_and(|short| short <= counter && !get_bit(&rooms, short)))
	};

	let strays = db["pduid_pdu"]
		.raw_keys()
		.ignore_err()
		.ready_fold(0_u64, |strays, key| {
			strays.saturating_add(stray(key.get(0..8).and_then(short_of)))
		})
		.await;

	// The search key carries the shortroomid twice: as the prefix and again
	// inside the pdu id behind the separator-terminated word.
	let strays = db["tokenids"]
		.raw_keys()
		.ignore_err()
		.ready_fold(strays, |strays, key| {
			let prefix = key.get(0..8).and_then(short_of);
			let embedded = key.get(8..).and_then(pdu_shortroomid);

			strays
				.saturating_add(stray(prefix))
				.saturating_add(stray(embedded))
		})
		.await;

	// Sending-queue keys hold a pdu id behind the destination only when
	// the value is empty; nonempty rows queue EDUs.
	let current = db["servercurrentevent_data"]
		.raw_stream()
		.ignore_err();

	let strays = db["servernameevent_data"]
		.raw_stream()
		.ignore_err()
		.chain(current)
		.ready_fold(strays, |strays, (key, value)| {
			let pdu = value.is_empty().and_then(|| pdu_shortroomid(key));

			strays.saturating_add(stray(pdu))
		})
		.await;

	db["useridcount_notification"]
		.raw_stream()
		.ignore_err()
		.ready_fold(strays, |strays, (_, value)| {
			let sroomid = serde_json::from_slice(value)
				.ok()
				.map(|notification: Notification| notification.sroomid);

			strays.saturating_add(stray(sroomid))
		})
		.await
}

/// Extracts the shortroomid of a pdu id sitting behind a separator.
///
/// The pdu id must have the 16-byte normal or 24-byte backfilled width;
/// anything else yields nothing.
fn pdu_shortroomid(bytes: &[u8]) -> Option<u64> {
	let sep = bytes.iter().position(|&byte| byte == SEP)?;
	let id = bytes.get(sep.saturating_add(1)..)?;

	(id.len() == 16 || id.len() == 24)
		.and_then(|| id.get(0..8))
		.and_then(short_of)
}

pub(super) fn short_of(bytes: &[u8]) -> Option<u64> {
	bytes.try_into().ok().map(u64::from_be_bytes)
}

fn bits_of(shorts: &[u64], words: usize) -> Bits {
	shorts
		.iter()
		.fold(vec![0_u64; words], |mut bits, short| {
			set_bit(&mut bits, *short);

			bits
		})
}

fn disposable(bytes: &[u8], event_stale: &[u64], statekey_stale: &[u64]) -> bool {
	!bytes.len().is_multiple_of(size_of::<u64>())
		|| references(bytes, event_stale, statekey_stale)
}

fn references(bytes: &[u8], event_stale: &[u64], statekey_stale: &[u64]) -> bool {
	let mut shorts = bytes
		.as_chunks::<{ size_of::<u64>() }>()
		.0
		.iter()
		.copied()
		.map(u64::from_be_bytes);

	Iterator::any(&mut shorts, |short| {
		get_bit(event_stale, short) || get_bit(statekey_stale, short)
	})
}

fn set_bit(bits: &mut [u64], index: u64) {
	if let Some(word) = usize::try_from(index / 64)
		.ok()
		.and_then(|word| bits.get_mut(word))
	{
		*word |= 1_u64 << (index % 64);
	}
}

fn get_bit(bits: &[u64], index: u64) -> bool {
	usize::try_from(index / 64)
		.ok()
		.and_then(|word| bits.get(word))
		.is_some_and(|word| word & (1_u64 << (index % 64)) != 0)
}

#[cfg(test)]
mod tests {
	use std::{collections::BTreeSet, sync::Arc};

	use tuwunel_core::err;
	use tuwunel_database::serialize_to_vec;

	use super::{
		Candidate, Counts, Diffs, Family, Identity, Reason, Resolved, Scan, StateDiff,
		by_identity, by_short, contenders, intersecting, resolution,
	};

	fn candidate(short: u64, identity: &[u8]) -> Candidate {
		(short, Identity::from_slice(identity))
	}

	fn statediff(parent: u64, statekey: u64, event: u64) -> [u8; 24] {
		let mut value = [0_u8; 24];
		value[..8].copy_from_slice(&parent.to_be_bytes());
		value[8..16].copy_from_slice(&statekey.to_be_bytes());
		value[16..].copy_from_slice(&event.to_be_bytes());

		value
	}

	#[test]
	fn contenders_counts_two_forward_rows_claiming_one_short() {
		let mut dangling = vec![candidate(7, b"$a"), candidate(7, b"$b"), candidate(9, b"$c")];

		assert_eq!(contenders(&mut dangling, by_short), 1);
	}

	#[test]
	fn contenders_counts_two_reverse_rows_naming_one_identity() {
		let mut promotable = vec![candidate(7, b"$a"), candidate(9, b"$a"), candidate(11, b"$b")];

		assert_eq!(contenders(&mut promotable, by_identity), 1);
	}

	#[test]
	fn contenders_is_zero_when_every_slot_is_claimed_once() {
		let mut dangling = vec![candidate(9, b"$a"), candidate(7, b"$b")];

		assert_eq!(contenders(&mut dangling, by_short), 0);
	}

	#[test]
	fn a_lone_dangling_winner_heals_without_refusing() {
		let family = Family {
			dangling: vec![candidate(7, b"$a")],
			..Default::default()
		};

		assert!(family.healable());
		assert!(!family.anomalous());
	}

	#[test]
	fn a_contended_short_refuses_instead_of_healing() {
		let family = Family {
			dangling: vec![candidate(7, b"$a"), candidate(7, b"$b")],
			contended: 1,
			..Default::default()
		};

		assert!(!family.healable());
		assert!(family.anomalous());
	}

	#[test]
	fn a_malformed_key_withholds_the_heal() {
		let family = Family {
			dangling: vec![candidate(7, b"$a")],
			malformed: 1,
			..Default::default()
		};

		assert!(!family.healable());
	}

	#[test]
	fn an_unresolved_row_withholds_the_promotion() {
		let family = Family {
			promotable: vec![candidate(7, b"$a")],
			unresolved: 1,
			..Default::default()
		};

		assert!(!family.healable());
		assert!(family.anomalous());
	}

	#[test]
	fn a_forward_read_error_propagates() {
		let result = resolution(Err(err!(Database("test read failure"))));

		assert!(result.is_err());
	}

	#[test]
	fn a_missing_forward_row_remains_absent() {
		let result = resolution(Err(err!(Request(NotFound("test row")))));

		assert!(matches!(result, Ok(Resolved::Absent)));
	}

	#[test]
	fn a_descendant_of_an_infected_state_refuses_the_repair() {
		let event_stale = [1_u64 << 7];
		let empty = [0_u64];
		let walk = Diffs {
			counter: 0,
			event_stale: &event_stale,
			statekey_stale: &empty,
			event_reverse: &empty,
			statekey_reverse: &empty,
		};

		let infected = statediff(0, 3, 7);
		let child = statediff(10, 4, 8);
		let counts = walk.row(Counts::default(), &10_u64.to_be_bytes(), &infected);
		let counts = walk.row(counts, &11_u64.to_be_bytes(), &child);

		assert!(counts.infected.contains(&10));
		assert_eq!(counts.infected_parents, 1);

		let scan = Scan {
			events: Family { losers: vec![7], ..Default::default() },
			infected_parents: counts.infected_parents,
			..Default::default()
		};

		assert!(scan.anomalous());
	}

	#[test]
	fn an_intersecting_infected_row_refuses_the_repair() {
		let scan = Scan {
			events: Family { losers: vec![7], ..Default::default() },
			colliding_diffs: 1,
			..Default::default()
		};

		assert!(scan.anomalous());
		assert!(!scan.family_anomalous());
	}

	#[test]
	fn runs_sharing_an_entry_intersect() {
		let diff = StateDiff {
			parent: None,
			added: Arc::new(BTreeSet::from([[1_u8; 16], [2_u8; 16]])),
			removed: Arc::new(BTreeSet::from([[2_u8; 16], [3_u8; 16]])),
		};

		assert!(intersecting(&diff));
	}

	#[test]
	fn disjoint_runs_do_not_intersect() {
		let diff = StateDiff {
			parent: None,
			added: Arc::new(BTreeSet::from([[1_u8; 16]])),
			removed: Arc::new(BTreeSet::from([[2_u8; 16]])),
		};

		assert!(!intersecting(&diff));
	}

	#[test]
	fn a_decline_record_serializes_as_contiguous_be_words() {
		let scan = Scan {
			counter: 7,
			colliding_diffs: 9,
			..Default::default()
		};

		let record = scan.decline_record(Reason::Unverifiable);
		let bytes = serialize_to_vec(&record).expect("record serializes");

		let expected: Vec<u8> = record
			.iter()
			.flat_map(|word| word.to_be_bytes())
			.collect();

		assert_eq!(bytes, expected);
		assert_eq!(bytes.len(), 22 * 8);
	}

	#[test]
	fn a_decline_record_packs_the_counters_in_layout_order() {
		let scan = Scan {
			counter: 2,
			events: Family {
				rows: 10,
				losers: vec![0; 11],
				dangling: vec![candidate(0, b"$a"); 12],
				promotable: vec![candidate(0, b"$a"); 13],
				contended: 14,
				unresolved: 15,
				malformed: 16,
				..Default::default()
			},
			statekeys: Family {
				rows: 20,
				losers: vec![0; 21],
				dangling: vec![candidate(0, b"$a"); 22],
				promotable: vec![candidate(0, b"$a"); 23],
				contended: 24,
				unresolved: 25,
				malformed: 26,
				..Default::default()
			},
			infected: (0..30).collect(),
			infected_parents: 31,
			orphans: 32,
			malformed_diffs: 33,
			colliding_diffs: 34,
			..Default::default()
		};

		let record = scan.decline_record(Reason::DeepAnomalous);
		let expected: [u64; 22] = [
			1, 4, 2, 10, 11, 12, 13, 14, 15, 16, 20, 21, 22, 23, 24, 25, 26, 30, 31, 32, 33, 34,
		];

		assert_eq!(record.as_slice(), expected.as_slice());
	}

	#[test]
	fn decline_reasons_keep_their_record_numbers() {
		assert_eq!(u64::from(Reason::Unverifiable), 1);
		assert_eq!(u64::from(Reason::Healable), 2);
		assert_eq!(u64::from(Reason::FamilyAnomalous), 3);
		assert_eq!(u64::from(Reason::DeepAnomalous), 4);
	}

	#[test]
	fn a_statediff_parent_must_precede_its_child() {
		let empty = [0_u64];
		let walk = Diffs {
			counter: 0,
			event_stale: &empty,
			statekey_stale: &empty,
			event_reverse: &empty,
			statekey_reverse: &empty,
		};
		let value = statediff(11, 3, 8);

		let counts = walk.row(Counts::default(), &10_u64.to_be_bytes(), &value);

		assert_eq!(counts.malformed, 1);
	}
}
