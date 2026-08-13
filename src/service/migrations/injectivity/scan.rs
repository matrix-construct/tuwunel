use std::{
	collections::{BTreeMap, BTreeSet},
	sync::Arc,
};

use futures::StreamExt;
use serde::Deserialize;
use tuwunel_core::{
	Result, debug, err, implement, info,
	smallvec::SmallVec,
	utils::{
		BoolExt, ReadyExt,
		stream::{IterStream, TryIgnore},
	},
	warn,
};
use tuwunel_database::{Database, Get, Handle, Map, SEP};

use crate::{Services, rooms::pdu_metadata::typed_relations::Key as RelationKey};

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

/// Reverse rows no forward value claims, paired with the identities they
/// name.
type Candidates = Vec<(u64, Identity)>;

/// One family's resolution: losers, the winner each maps to, and the
/// count that resolved to nothing.
type Resolution = (Vec<u64>, BTreeMap<u64, u64>, u64);

/// Exclusive upper bound on verifiable short ids.
///
/// Each scan bitmap costs one bit per id up to the global counter, and the
/// deep sweep holds up to five at once. At or above this bound the scan
/// reports unverifiable instead.
const MAX_SHORT: u64 = 1 << 30;

/// One family's residue: its losers, their winners, and the anomaly counts
/// that impugn the scan.
///
/// The losers include any unresolved ones, so `winners` is total exactly
/// when `unresolved` is zero.
#[derive(Default)]
pub(super) struct Family {
	pub(super) rows: u64,
	pub(super) losers: Vec<u64>,
	pub(super) winners: BTreeMap<u64, u64>,
	pub(super) dangling: u64,
	pub(super) unresolved: u64,
	pub(super) malformed: u64,
}

/// Everything one scan measured, and the worklists the repair consumes.
///
/// The deep counts stay zero when neither family has a loser, since the
/// deeper indexes are not read in that case.
#[derive(Default)]
pub(super) struct Scan {
	pub(super) events: Family,
	pub(super) statekeys: Family,
	pub(super) dirty: u64,
	pub(super) entries: u64,
	pub(super) infected: BTreeSet<u64>,
	pub(super) orphans: u64,
	pub(super) missing_parents: u64,
	pub(super) infected_parents: u64,
	pub(super) malformed_diffs: u64,
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
	ghosts: u64,
	removed_ghosts: u64,
	orphans: u64,
	missing_parents: u64,
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
/// stale. The deeper indexes are read only when a loser exists.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn scan(services: &Services) -> Result<Scan> {
	info!("Scanning ShortID columns for duplicate values...");

	let counter = services.globals.current_count();

	if counter >= MAX_SHORT {
		warn!(
			%counter,
			"Short id space too large to verify injectivity; stale auth chain caches, if \
			 any, survive and can distort state resolution until `server clear-caches`."
		);
		return Ok(Scan { unverifiable: true, ..Default::default() });
	}

	let words = usize::try_from((counter / 64).saturating_add(1))
		.map_err(|_| err!("short id bitmap exceeds the address width"))?;

	let (events, event_reverse) =
		family(services, "eventid_shorteventid", "shorteventid_eventid", counter, words).await;

	let (statekeys, statekey_reverse) =
		family(services, "statekey_shortstatekey", "shortstatekey_statekey", counter, words)
			.await;

	if events.losers.is_empty() && statekeys.losers.is_empty() {
		return Ok(Scan { events, statekeys, ..Default::default() });
	}

	let swept =
		sweep(services, &events, &statekeys, event_reverse, statekey_reverse, counter).await;

	let scan = Scan { events, statekeys, ..swept };

	// The stray census is the widest pass and gates nothing; it reports on
	// the boot that acts and skips the rescans a refusal causes.
	match scan.anomalous() {
		| true => Ok(scan),
		| false => {
			let strays = strays(&services.db, counter, words).await;

			Ok(Scan { strays, ..scan })
		},
	}
}

/// Whether any count impugns the scan or exceeds what the repair handles.
///
/// Any anomaly refuses the destructive repair lane; the cache-clearing
/// lane is unconditionally safe and proceeds regardless.
#[implement(Scan)]
pub(super) fn anomalous(&self) -> bool {
	self.events.dangling > 0
		|| self.statekeys.dangling > 0
		|| self.events.unresolved > 0
		|| self.statekeys.unresolved > 0
		|| self.events.malformed > 0
		|| self.statekeys.malformed > 0
		|| self.orphans > 0
		|| self.missing_parents > 0
		|| self.infected_parents > 0
		|| self.malformed_diffs > 0
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
) -> (Family, Bits) {
	let db = &services.db;

	let (reverse_bits, rows, reverse_malformed) = reverse_bitmap(&db[reverse], words).await;

	let (forward_bits, dangling, forward_malformed) =
		dangling_winners(&db[forward], &reverse_bits, counter, words).await;

	// A set reverse bit no forward value claims is what the pass collects.
	let candidates = match any_unclaimed(&reverse_bits, &forward_bits, counter) {
		| false => Candidates::new(),
		| true => loser_candidates(&db[reverse], &forward_bits, counter).await,
	};

	drop(forward_bits);

	let (losers, winners, unresolved) = resolve(&db[forward], &candidates).await;

	let family = Family {
		rows,
		losers,
		winners,
		dangling,
		unresolved,
		malformed: reverse_malformed.saturating_add(forward_malformed),
	};

	debug!(
		rows = family.rows,
		losers = family.losers.len(),
		dangling = family.dangling,
		unresolved = family.unresolved,
		malformed = family.malformed,
		"Scanned one short id family."
	);

	info!(?forward, ?reverse, "Finished scanning column pair.",);

	(family, reverse_bits)
}

/// Streams a reverse map into its keyset bitmap, its row count, and its
/// count of keys that are not an 8-byte short id.
///
/// The rows are counted here rather than in the loser pass, which a clean
/// family skips.
async fn reverse_bitmap(map: &Arc<Map>, words: usize) -> (Bits, u64, u64) {
	map.raw_keys()
		.ignore_err()
		.ready_fold((vec![0_u64; words], 0_u64, 0_u64), |(mut bits, rows, malformed), key| {
			let rows = rows.saturating_add(1);

			match short_of(key) {
				| None => (bits, rows, malformed.saturating_add(1)),
				| Some(short) => {
					set_bit(&mut bits, short);

					(bits, rows, malformed)
				},
			}
		})
		.await
}

/// Streams a forward map against the reverse bitmap for dangling winners.
///
/// A dangling winner is a forward value no reverse row answers for.
/// Values past the counter are concurrent allocations, not danglings.
async fn dangling_winners(
	map: &Arc<Map>,
	reverse_bits: &[u64],
	counter: u64,
	words: usize,
) -> (Bits, u64, u64) {
	map.raw_stream()
		.ignore_err()
		.ready_fold(
			(vec![0_u64; words], 0_u64, 0_u64),
			|(mut bits, dangling, malformed), (_, value)| match short_of(value) {
				| None => (bits, dangling, malformed.saturating_add(1)),
				| Some(short) => {
					let dangling = dangling.saturating_add(u64::from(
						short <= counter && !get_bit(reverse_bits, short),
					));

					set_bit(&mut bits, short);

					(bits, dangling, malformed)
				},
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
async fn loser_candidates(map: &Arc<Map>, forward_bits: &[u64], counter: u64) -> Candidates {
	map.raw_stream()
		.ignore_err()
		.ready_fold(Candidates::new(), |mut candidates, (key, value)| {
			let unclaimed =
				short_of(key).filter(|short| *short <= counter && !get_bit(forward_bits, *short));

			if let Some(short) = unclaimed {
				candidates.push((short, Identity::from_slice(value)));
			}

			candidates
		})
		.await
}

/// Dereferences each candidate's identity to split losers from winners.
///
/// The identity a loser's reverse row names must hold a live forward row,
/// whose value is the winner. A candidate resolving to itself was a
/// concurrent allocation, not a loser.
async fn resolve(map: &Arc<Map>, candidates: &[(u64, Identity)]) -> Resolution {
	let (mut losers, winners, unresolved, paired) = candidates
		.iter()
		.map(candidate_identity)
		.stream()
		.get(map)
		.map(resolution)
		.zip(candidates.iter().map(candidate_short).stream())
		.ready_fold(
			(Vec::new(), BTreeMap::new(), 0_u64, 0_usize),
			|(mut losers, mut winners, unresolved, paired), (winner, loser)| {
				let paired = paired.saturating_add(1);

				match winner {
					| Some(winner) if winner == loser => (losers, winners, unresolved, paired),
					| Some(winner) => {
						losers.push(loser);
						winners.insert(loser, winner);

						(losers, winners, unresolved, paired)
					},
					| None => {
						losers.push(loser);

						(losers, winners, unresolved.saturating_add(1), paired)
					},
				}
			},
		)
		.await;

	// A batched lookup can compress a failed chunk into one error item,
	// desynchronizing the zip; the unpaired tail stays unresolved so the
	// refusal gate holds.
	let tail = candidates.get(paired..).unwrap_or_default();
	losers.extend(tail.iter().map(candidate_short));

	let unresolved = unresolved.saturating_add(u64::try_from(tail.len()).unwrap_or(u64::MAX));

	(losers, winners, unresolved)
}

// Named for the higher-ranked closure generality the dereference stream
// needs; an inline closure pins the item lifetimes.
fn candidate_identity((_, identity): &(u64, Identity)) -> &Identity { identity }

fn candidate_short((short, _): &(u64, Identity)) -> u64 { *short }

fn resolution(result: Result<Handle<'_>>) -> Option<u64> {
	result.ok().as_deref().and_then(short_of)
}

/// Reads the deeper indexes once a loser exists in either family.
///
/// Statediff entries are tested against both families and chain-cache rows
/// against either, while the shortroomid families are counted for the
/// report without gating any repair.
#[tracing::instrument(level = "debug", skip_all)]
async fn sweep(
	services: &Services,
	events: &Family,
	statekeys: &Family,
	event_reverse: Bits,
	statekey_reverse: Bits,
	counter: u64,
) -> Scan {
	let db = &services.db;
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

	let counts = diffs(db, words, walk).await;

	drop(event_reverse);
	drop(statekey_reverse);

	// A descendant of an infected state would need re-derivation down the
	// diff chain, which is not built; the anomaly refuses the destructive
	// lane instead.
	let infected_parents = match counts.infected.is_empty() {
		| true => 0,
		| false =>
			db["shortstatehash_statediff"]
				.raw_stream()
				.ignore_err()
				.ready_fold(0_u64, |descendants, (_, value)| {
					let parent = value.get(0..8).and_then(short_of);

					descendants.saturating_add(u64::from(
						parent.is_some_and(|parent| counts.infected.contains(&parent)),
					))
				})
				.await,
	};

	// Chains are dedup'd in the short id domain, so a stale reference in a
	// key or a value poisons the row either way.
	let authchain = db["shorteventid_authchain"]
		.raw_stream()
		.ignore_err();

	let (dirty, entries) = db["authchainkey_authchain"]
		.raw_stream()
		.ignore_err()
		.chain(authchain)
		.ready_fold((0_u64, 0_u64), |(dirty, entries), (key, chain)| {
			let hit = references(key, &event_stale, &statekey_stale)
				|| references(chain, &event_stale, &statekey_stale);

			(dirty.saturating_add(u64::from(hit)), entries.saturating_add(1))
		})
		.await;

	// ready_fold rather than ready_filter_map: the higher-ranked adapter
	// fails the boot coroutine's Send obligation over cursor-borrowed items.
	let moves: Vec<u64> = db["shorteventid_shortstatehash"]
		.raw_keys()
		.ignore_err()
		.ready_fold(Vec::new(), |mut moves, key| {
			if let Some(loser) = short_of(key).filter(|short| get_bit(&event_stale, *short)) {
				moves.push(loser);
			}

			moves
		})
		.await;

	let relations: Relations = db["relatesto_typed"]
		.raw_stream()
		.ignore_err()
		.ready_fold(Relations::new(), |mut relations, (key, value)| {
			let dirty = short_of(value)
				.filter(|loser| get_bit(&event_stale, *loser))
				.zip(RelationKey::try_from(key).ok());

			if let Some((loser, key)) = dirty {
				relations.push((key, loser));
			}

			relations
		})
		.await;

	warn!(
		dirty,
		entries,
		infected = counts.infected.len(),
		ghosts = counts.ghosts,
		removed_ghosts = counts.removed_ghosts,
		orphans = counts.orphans,
		missing_parents = counts.missing_parents,
		infected_parents,
		malformed_diffs = counts.malformed,
		moves = moves.len(),
		relations = relations.len(),
		"Swept the deeper short id indexes."
	);

	Scan {
		dirty,
		entries,
		infected: counts.infected,
		orphans: counts.orphans,
		missing_parents: counts.missing_parents,
		infected_parents,
		malformed_diffs: counts.malformed,
		moves,
		relations,
		..Default::default()
	}
}

/// Folds every statediff row through the walk, its parent keyset first.
///
/// The whole keyset must precede the row walk, a row's parent appearing
/// anywhere in the file.
async fn diffs(db: &Database, words: usize, walk: Diffs<'_>) -> Counts {
	let parents = db["shortstatehash_statediff"]
		.raw_keys()
		.ignore_err()
		.ready_fold(vec![0_u64; words], |mut bits, key| {
			if let Some(short) = short_of(key) {
				set_bit(&mut bits, short);
			}

			bits
		})
		.await;

	db["shortstatehash_statediff"]
		.raw_stream()
		.ignore_err()
		.ready_fold(Counts::default(), |counts, (key, value)| {
			walk.row(counts, key, value, &parents)
		})
		.await
}

impl Diffs<'_> {
	/// Folds one statediff row through the walk.
	///
	/// The value carries an 8-byte parent, then 16-byte entries of a
	/// statekey and an event half, an added run first and a removed run
	/// only behind an 8-byte zero sentinel. The sentinel shifts entry
	/// alignment by 8, so the walk is sequential rather than chunked.
	fn row(&self, mut counts: Counts, key: &[u8], value: &[u8], parents: &[u64]) -> Counts {
		let (Some(row), Some(parent)) = (short_of(key), value.get(0..8).and_then(short_of))
		else {
			counts.malformed = counts.malformed.saturating_add(1);
			return counts;
		};

		if parent != 0 && parent <= self.counter && !get_bit(parents, parent) {
			counts.missing_parents = counts.missing_parents.saturating_add(1);
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

/// Counts shortroomid references with no forward row.
///
/// Purged rooms and losing allocations both produce them; no repair step
/// touches a shortroomid family, so the count reports and gates nothing.
#[tracing::instrument(level = "debug", skip_all)]
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

fn references(bytes: &[u8], event_stale: &[u64], statekey_stale: &[u64]) -> bool {
	bytes
		.as_chunks::<{ size_of::<u64>() }>()
		.0
		.iter()
		.copied()
		.map(u64::from_be_bytes)
		.any(|short| get_bit(event_stale, short) || get_bit(statekey_stale, short))
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
