use std::{collections::BTreeMap, sync::Arc};

use tuwunel_core::{
	Result, debug, info,
	smallvec::SmallVec,
	utils::{TryReadyExt, hash::sha256::Digest},
	warn,
};
use tuwunel_database::{Map, Txn};

use super::{
	Reason, Verdict, clear_chains,
	scan::{Family, Scan, short_of},
};
use crate::{
	Services,
	rooms::state_compressor::{
		CompressedState, CompressedStateEvent, StateDiff, compress_state_event,
		parse_compressed_state_event,
	},
};

/// The digest rows naming each infected state, keyed by the state.
///
/// A digest key of any other width is unreachable by digest lookup and so
/// cannot misdirect a dedup; only the 32-byte rows are collected.
type Digests = BTreeMap<u64, SmallVec<[Digest; 1]>>;

/// What one family's heal staged.
///
/// A reinstated row is a dangling winner regaining the reverse row its
/// forward row already names; a promoted row is an unresolved loser
/// regaining the forward row its identity lost.
#[derive(Default)]
struct Healed {
	reinstated: usize,
	promoted: usize,
}

/// What patching one statediff row computed, before it is written back.
///
/// The change counts say how many entries each run rewrote winner-ward;
/// `shrunk` counts duplicates that converged inside one run; `colliding`
/// counts entries subtracted from the removed run because the patch left
/// them in both.
struct Patched {
	added: CompressedState,
	removed: CompressedState,
	added_changes: u64,
	removed_changes: u64,
	shrunk: usize,
	colliding: usize,
}

/// Completes the torn writes the residue names on its own.
///
/// The pre-fix allocator put the forward and reverse rows separately, so a
/// lost tail write leaves one half of a pair behind: a dangling winner has
/// the forward row and wants its reverse row back, a promotable loser has
/// the reverse row and wants the forward row its identity lost. Returns
/// whether anything was written, which is the caller's signal to rescan
/// before judging the counts a heal changes.
///
/// Never run this and [`repair`] in the same pass: a promoted row is also
/// a loser, so `delete_losers` would remove the reverse row the promotion
/// just completed.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) fn heal(services: &Services, scan: &Scan) -> bool {
	if scan.unverifiable || !scan.healable() {
		return false;
	}

	let db = &services.db;
	let mut txn = db.txn();

	let event_reverse = &db["shorteventid_eventid"];
	let event_forward = &db["eventid_shorteventid"];
	let events = heal_family(&mut txn, event_reverse, event_forward, &scan.events);

	let statekey_reverse = &db["shortstatekey_statekey"];
	let statekey_forward = &db["statekey_shortstatekey"];
	let statekeys = heal_family(&mut txn, statekey_reverse, statekey_forward, &scan.statekeys);

	info!(
		reinstated_events = events.reinstated,
		promoted_events = events.promoted,
		reinstated_statekeys = statekeys.reinstated,
		promoted_statekeys = statekeys.promoted,
		"Completing torn short id writes; rescanning to re-measure what they explain."
	);

	txn.execute();

	true
}

/// Stages one family's reinstatements and promotions.
///
/// A family any anomaly impugns stages nothing, since the bitmaps naming
/// its residue are the ones in doubt. Returns the counts staged.
fn heal_family(txn: &mut Txn, reverse: &Map, forward: &Map, family: &Family) -> Healed {
	if !family.healable() {
		return Healed::default();
	}

	for (short, identity) in &family.dangling {
		txn.insert_raw(reverse, short.to_be_bytes(), identity.as_slice());
	}

	for (short, identity) in &family.promotable {
		txn.insert_raw(forward, identity.as_slice(), short.to_be_bytes());
	}

	Healed {
		reinstated: family.dangling.len(),
		promoted: family.promotable.len(),
	}
}

/// Applies whatever repair the scan cleared, in hazard order.
///
/// The cache-clearing lane runs when the deep scan finds dirt or a verdict
/// skips that scan, and is unconditionally safe. The destructive lane runs
/// only when no anomaly impugned the scan. A decline logs its residue and
/// names its reason, so the caller's stamp records the counters; only an
/// error withholds the stamp.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn repair(
	services: &Services,
	scan: &Scan,
	chains_cleared_this_boot: bool,
) -> Result<Verdict> {
	let healable = scan.healable();
	let family_anomalous = scan.family_anomalous();
	let needs_chain_clear = scan.unverifiable || healable || family_anomalous;

	if !chains_cleared_this_boot && needs_chain_clear {
		warn!(
			"Short id verdict skipped the deep auth chain census; clearing the cache before \
			 finalizing."
		);

		clear_chains(services).await?;
	}

	if scan.unverifiable {
		return Ok(Verdict::Declined(Reason::Unverifiable));
	}

	if scan.strays > 0 {
		warn!(
			stray_references = scan.strays,
			"Short room id references without a forward row exist; nothing repairs them."
		);
	}

	if scan.dirty > 0 {
		warn!(
			dirty_entries = scan.dirty,
			total_entries = scan.entries,
			"Cached auth chains contain malformed or stale short id data; clearing the auth \
			 chain cache."
		);

		clear_chains(services).await?;
	}

	// A promoted row is also a loser, so repairing an unhealed residue
	// would delete the reverse row a promotion was about to complete.
	if healable {
		warn!(
			dangling_events = scan.events.dangling.len(),
			dangling_statekeys = scan.statekeys.dangling.len(),
			promotable_events = scan.events.promotable.len(),
			promotable_statekeys = scan.statekeys.promotable.len(),
			"Refusing the destructive short id repair; the heal passes did not settle. The \
			 residue is recorded and left in place. Please report this line upstream."
		);

		return Ok(Verdict::Declined(Reason::Healable));
	}

	if family_anomalous {
		warn!(
			dangling_events = scan.events.dangling.len(),
			dangling_statekeys = scan.statekeys.dangling.len(),
			promotable_events = scan.events.promotable.len(),
			promotable_statekeys = scan.statekeys.promotable.len(),
			contended_events = scan.events.contended,
			contended_statekeys = scan.statekeys.contended,
			unresolved_events = scan.events.unresolved,
			unresolved_statekeys = scan.statekeys.unresolved,
			malformed_event_keys = scan.events.malformed,
			malformed_statekey_keys = scan.statekeys.malformed,
			"Refusing the destructive short id repair; the family census contains an unhandled \
			 shape. The residue is recorded and left in place. Please report this line upstream."
		);

		return Ok(Verdict::Declined(Reason::FamilyAnomalous));
	}

	if scan.events.losers.is_empty() && scan.statekeys.losers.is_empty() {
		info!("Short id mappings verified injective.");

		return Ok(Verdict::Settled);
	}

	if scan.anomalous() {
		warn!(
			infected_parents = scan.infected_parents,
			orphan_entries = scan.orphans,
			malformed_diffs = scan.malformed_diffs,
			colliding_diffs = scan.colliding_diffs,
			"Refusing the destructive short id repair; the nonzero counts name shapes it does \
			 not handle. The residue is recorded and left in place. Please report this line \
			 upstream."
		);

		return Ok(Verdict::Declined(Reason::DeepAnomalous));
	}

	patch_statediffs(services, scan).await?;
	move_keys(services, scan).await?;
	delete_losers(services, scan);

	Ok(Verdict::Settled)
}

/// Patches the ghost halves of infected statediff entries to their winners.
///
/// Re-emitting through the sorted serialize path drops any entry the patch
/// makes a duplicate. The digest row naming each patched state rides the
/// same transaction: deleted, never recomputed, since a recomputed digest
/// could collide with an existing key and manufacture a duplicate state
/// this family has no detector for.
#[tracing::instrument(level = "debug", skip_all)]
async fn patch_statediffs(services: &Services, scan: &Scan) -> Result {
	if scan.infected.is_empty() {
		return Ok(());
	}

	let digests: Digests = services.db["statehash_shortstatehash"]
		.raw_stream()
		.ready_try_fold(Digests::new(), |mut digests, (key, value)| {
			if let Some(state) = short_of(value).filter(|state| scan.infected.contains(state))
				&& let Ok(digest) = key.try_into()
			{
				digests.entry(state).or_default().push(digest);
			}

			Ok(digests)
		})
		.await?;

	// Serial: each state's patch and digest delete form one transaction,
	// and the measured population is a handful of rows.
	for &state in &scan.infected {
		patch_state(services, scan, &digests, state).await?;
	}

	Ok(())
}

/// Patches one state's diff row, its digest row riding the transaction.
///
/// The pair lands together or not at all: a surviving digest row would
/// misdirect a later state dedup toward bytes the state no longer has.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%state,
	),
)]
async fn patch_state(services: &Services, scan: &Scan, digests: &Digests, state: u64) -> Result {
	let diff = services
		.state_compressor
		.get_statediff(state)
		.await?;

	let Patched {
		added,
		removed,
		added_changes,
		removed_changes,
		shrunk,
		colliding,
	} = patch_runs(&diff, scan);

	if removed_changes > 0 {
		warn!(
			%state,
			entries = removed_changes,
			"Patched ghost entries inside a removed run; the state resolves differently \
			 now that the removal matches."
		);
	}

	if shrunk > 0 {
		info!(
			%state,
			entries = shrunk,
			"Patching converged duplicate entries; the state shrank."
		);
	}

	if colliding > 0 {
		warn!(
			%state,
			entries = colliding,
			"Subtracted colliding entries from the removed run; each state key keeps its \
			 event rather than vanishing."
		);
	}

	let patched = StateDiff {
		parent: diff.parent,
		added: Arc::new(added),
		removed: Arc::new(removed),
	};

	let statehashes = &services.db["statehash_shortstatehash"];
	let mut txn = services.db.txn();

	// stateinfo_cache is not invalidated: migrations precede the workers
	// that populate it.
	services
		.state_compressor
		.save_statediff(&mut txn, state, &patched);

	digests
		.get(&state)
		.into_iter()
		.flatten()
		.for_each(|digest| txn.del_raw(statehashes, digest));

	txn.execute();

	info!(
		%state,
		entries = added_changes.saturating_add(removed_changes),
		"Patched stale short ids out of a compressed state."
	);

	Ok(())
}

/// Rebuilds one diff's runs winner-ward and subtracts the collision the
/// patch creates.
///
/// The runs are mapped independently, so a ghost in one run whose winner
/// sits in the other patches both to one entry, and applying added before
/// removed would then erase the state key. Subtracting the intersection
/// from removed is exact in both orientations: the row meant the key
/// holds this event before the patch, and it still does after. `shrunk`
/// is taken first, keeping its meaning to duplicates converging inside
/// one run.
fn patch_runs(diff: &StateDiff, scan: &Scan) -> Patched {
	let (added, added_changes) = patch(&diff.added, scan);
	let (patched_removed, removed_changes) = patch(&diff.removed, scan);

	let shrunk = diff
		.added
		.len()
		.saturating_add(diff.removed.len())
		.saturating_sub(added.len())
		.saturating_sub(patched_removed.len());

	let removed: CompressedState = patched_removed
		.difference(&added)
		.copied()
		.collect();

	let colliding = patched_removed
		.len()
		.saturating_sub(removed.len());

	Patched {
		added,
		removed,
		added_changes,
		removed_changes,
		shrunk,
		colliding,
	}
}

/// Maps both halves of each entry through the winner maps.
///
/// Returns the rebuilt set and the number of entries that changed; an
/// entry with no stale half passes through unchanged.
fn patch(entries: &CompressedState, scan: &Scan) -> (CompressedState, u64) {
	entries
		.iter()
		.fold((CompressedState::new(), 0_u64), |(mut patched, changes), entry| {
			let winner = winner_of(entry, scan);
			let changes = changes.saturating_add(u64::from(winner.is_some()));

			patched.insert(winner.unwrap_or(*entry));

			(patched, changes)
		})
}

/// Rebuilds one entry winner-ward, when either half is a loser.
///
/// An entry with no stale half yields nothing.
fn winner_of(entry: &CompressedStateEvent, scan: &Scan) -> Option<CompressedStateEvent> {
	let (statekey, event) = parse_compressed_state_event(*entry);
	let winner_statekey = scan.statekeys.winners.get(&statekey).copied();
	let winner_event = scan.events.winners.get(&event).copied();

	(winner_statekey.is_some() || winner_event.is_some()).then(|| {
		compress_state_event(winner_statekey.unwrap_or(statekey), winner_event.unwrap_or(event))
	})
}

/// Moves loser-keyed state rows to their winner key and rewrites
/// loser-valued relation rows.
///
/// A `relatesto_typed` value is no key and rewrites unconditionally; the
/// key-position policy lives on [`move_state_row`].
#[tracing::instrument(level = "debug", skip_all)]
async fn move_keys(services: &Services, scan: &Scan) -> Result {
	if scan.moves.is_empty() && scan.relations.is_empty() {
		return Ok(());
	}

	// Serial: the loser decisions feed one shared transaction, and the
	// measured population is zero to a handful of rows.
	let states = &services.db["shorteventid_shortstatehash"];
	let mut txn = services.db.txn();

	for &loser in &scan.moves {
		let Some(&winner) = scan.events.winners.get(&loser) else {
			continue;
		};

		move_state_row(states, &mut txn, loser, winner).await?;
	}

	let relations = &services.db["relatesto_typed"];

	for (key, loser) in &scan.relations {
		let Some(&winner) = scan.events.winners.get(loser) else {
			continue;
		};

		txn.insert_raw(relations, key, winner.to_be_bytes());
	}

	info!(
		moves = scan.moves.len(),
		relations = scan.relations.len(),
		"Rewrote loser-keyed and loser-valued rows."
	);

	txn.execute();

	Ok(())
}

/// Moves one loser-keyed state row toward its winner.
///
/// The value moves only onto an absent winner key; an occupied one was
/// written at another moment and keeps its own row.
async fn move_state_row(states: &Arc<Map>, txn: &mut Txn, loser: u64, winner: u64) -> Result {
	match states.get(&winner.to_be_bytes()).await {
		| Ok(_) =>
			debug!(loser, winner, "Dropping a loser-keyed state row; the winner has its own."),
		| Err(error) if error.is_not_found() => {
			let value = states.get(&loser.to_be_bytes()).await?;

			txn.insert_raw(states, winner.to_be_bytes(), &*value);
		},
		| Err(error) => return Err(error),
	}

	txn.del_raw(states, loser.to_be_bytes());

	Ok(())
}

/// Deletes the loser reverse rows of both families under one cork.
///
/// Last on purpose: uncorked, each removal would flush the write-ahead log
/// per key, and any earlier placement would destroy the resolver an
/// interrupted repair needs to resume.
fn delete_losers(services: &Services, scan: &Scan) {
	info!(
		stale_events = scan.events.losers.len(),
		stale_statekeys = scan.statekeys.losers.len(),
		"Deleting stale short id reverse rows."
	);

	let _cork = services.db.cork_and_sync();

	let events = &services.db["shorteventid_eventid"];

	for loser in &scan.events.losers {
		events.remove(&loser.to_be_bytes());
	}

	let statekeys = &services.db["shortstatekey_statekey"];

	for loser in &scan.statekeys.losers {
		statekeys.remove(&loser.to_be_bytes());
	}
}

#[cfg(test)]
mod tests {
	use std::{collections::BTreeMap, sync::Arc};

	use super::{CompressedState, Family, Scan, StateDiff, compress_state_event, patch_runs};

	fn ghost_scan(loser: u64, winner: u64) -> Scan {
		Scan {
			events: Family {
				losers: vec![loser],
				winners: BTreeMap::from([(loser, winner)]),
				..Default::default()
			},
			..Default::default()
		}
	}

	fn diff_of(added: &[(u64, u64)], removed: &[(u64, u64)]) -> StateDiff {
		let compress = |entries: &[(u64, u64)]| -> CompressedState {
			entries
				.iter()
				.map(|&(statekey, event)| compress_state_event(statekey, event))
				.collect()
		};

		StateDiff {
			parent: None,
			added: Arc::new(compress(added)),
			removed: Arc::new(compress(removed)),
		}
	}

	#[test]
	fn a_ghost_added_with_its_winner_removed_keeps_the_state_key() {
		let scan = ghost_scan(7, 3);
		let diff = diff_of(&[(5, 7)], &[(5, 3)]);

		let patched = patch_runs(&diff, &scan);
		let winner_entry = compress_state_event(5, 3);

		assert_eq!(patched.colliding, 1);
		assert_eq!(patched.shrunk, 0);
		assert_eq!(patched.added_changes, 1);
		assert_eq!(patched.removed_changes, 0);
		assert!(patched.added.contains(&winner_entry));
		assert!(patched.removed.is_empty());
	}

	#[test]
	fn a_ghost_removed_with_its_winner_added_keeps_the_state_key() {
		let scan = ghost_scan(7, 3);
		let diff = diff_of(&[(5, 3)], &[(5, 7)]);

		let patched = patch_runs(&diff, &scan);
		let winner_entry = compress_state_event(5, 3);

		assert_eq!(patched.colliding, 1);
		assert_eq!(patched.shrunk, 0);
		assert_eq!(patched.added_changes, 0);
		assert_eq!(patched.removed_changes, 1);
		assert!(patched.added.contains(&winner_entry));
		assert!(patched.removed.is_empty());
	}
}
