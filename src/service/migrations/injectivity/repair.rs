use std::{collections::BTreeMap, sync::Arc};

use tuwunel_core::{
	Result, debug, info,
	smallvec::SmallVec,
	utils::{ReadyExt, hash::sha256::Digest, stream::TryIgnore},
	warn,
};
use tuwunel_database::{Map, Txn};

use super::{
	clear_chains,
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
/// The cache-clearing lane runs on any dirty chain and is unconditionally
/// safe; the destructive lane runs only when no anomaly impugned the scan.
/// Returns whether the residue settled: a refusal reports false so the
/// caller leaves the marker unwritten and the next boot scans again, while
/// an anomaly with nothing to repair settles with a warning instead.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn repair(services: &Services, scan: &Scan) -> Result<bool> {
	if scan.unverifiable {
		return Ok(false);
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

		clear_chains(services).await;
	}

	// Refused rather than repaired, so the cache-clearing lane above still
	// runs on a boot whose heals never settled. A promoted row is also a
	// loser, so repairing an unhealed residue would delete the reverse row
	// a promotion was about to complete.
	if scan.healable() {
		warn!(
			dangling_events = scan.events.dangling.len(),
			dangling_statekeys = scan.statekeys.dangling.len(),
			promotable_events = scan.events.promotable.len(),
			promotable_statekeys = scan.statekeys.promotable.len(),
			"Short id heals did not settle; refusing the repair while residue stays healable."
		);

		return Ok(false);
	}

	if scan.events.losers.is_empty() && scan.statekeys.losers.is_empty() {
		match scan.anomalous() {
			| false => info!("Short id mappings verified injective."),
			| true => warn!(
				dangling_events = scan.events.dangling.len(),
				dangling_statekeys = scan.statekeys.dangling.len(),
				contended_events = scan.events.contended,
				contended_statekeys = scan.statekeys.contended,
				unresolved_events = scan.events.unresolved,
				unresolved_statekeys = scan.statekeys.unresolved,
				malformed_event_keys = scan.events.malformed,
				malformed_statekey_keys = scan.statekeys.malformed,
				"Short id anomalies exist with no stale mappings to repair; not scanning again."
			),
		}

		return Ok(true);
	}

	if scan.anomalous() {
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
			orphan_entries = scan.orphans,
			missing_parents = scan.missing_parents,
			infected_parents = scan.infected_parents,
			malformed_diffs = scan.malformed_diffs,
			"Refusing the destructive short id repair; the nonzero counts name shapes it does \
			 not handle. Please report this line upstream, since the scan repeats each boot \
			 until a release handles them."
		);

		return Ok(false);
	}

	patch_statediffs(services, scan).await?;
	move_keys(services, scan).await?;
	delete_losers(services, scan);

	Ok(true)
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
		.ignore_err()
		.ready_fold(Digests::new(), |mut digests, (key, value)| {
			let infected = short_of(value)
				.filter(|state| scan.infected.contains(state))
				.zip(key.try_into().ok());

			if let Some((state, digest)) = infected {
				digests.entry(state).or_default().push(digest);
			}

			digests
		})
		.await;

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

	let (added, added_changes) = patch(&diff.added, scan);
	let (removed, removed_changes) = patch(&diff.removed, scan);

	if removed_changes > 0 {
		warn!(
			%state,
			entries = removed_changes,
			"Patched ghost entries inside a removed run; the state resolves differently \
			 now that the removal matches."
		);
	}

	let shrunk = diff
		.added
		.len()
		.saturating_add(diff.removed.len())
		.saturating_sub(added.len())
		.saturating_sub(removed.len());

	if shrunk > 0 {
		info!(
			%state,
			entries = shrunk,
			"Patching converged duplicate entries; the state shrank."
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
