//! Short id injectivity: the one-time scan and repair.
//!
//! Releases before v1.8.3 could mint two short ids for one identity,
//! leaving stale reverse rows in both families, ghost entries in a few
//! compressed states, and auth chains cached from both allocations. The
//! migration measures that residue, repairs what matches the shapes it
//! handles, and marks itself complete like any other. Every release
//! through v1.8.3 also memoized auth chains truncated at a missing
//! ancestor, which no scan tells from a whole one, so the cache is
//! discarded once on a marker of its own.

mod repair;
mod scan;

use tuwunel_core::{Result, result::LogErr, utils::TryReadyExt, warn};

use self::{
	repair::{heal, repair},
	scan::scan,
};
use crate::Services;

/// Global marker recording the scan and repair reached a verdict.
///
/// A decline stamps it like a settled repair, so no residue causes a
/// rescan on later boots; the decline's counters become the value where
/// a settled repair leaves it empty. Only an error leaves it unwritten,
/// and an error fails the boot. Every reader tests presence only.
static MARKER: &[u8] = b"fix_short_injectivity";

/// Global marker recording the one-time auth chain cache clear.
///
/// Gating the clear on [`MARKER`] would re-run it on every boot an
/// errored repair leaves unstamped.
static CLEAR_MARKER: &[u8] = b"clear_auth_chain_cache";

/// Scan passes one boot allows before giving up on convergence.
///
/// A heal completes torn writes and rescans to re-measure what they
/// explain. Early passes may heal, while the final pass either repairs the
/// settled residue or declines one that remains healable.
const PASSES: usize = 3;

/// The verdict [`repair`] reaches; the caller stamps [`MARKER`] on any
/// verdict.
///
/// Only an error escapes without a verdict, leaving the marker unwritten
/// and failing the boot.
enum Verdict {
	Settled,
	Declined(Reason),
}

/// Why a repair declined, numbered for the decline record.
///
/// The reason decides which counters of the record were measured: an
/// unverifiable scan measured nothing past the counter, a healable or
/// family-anomalous verdict measured the families only, and a deep
/// anomaly measured everything.
enum Reason {
	Unverifiable = 1,
	Healable = 2,
	FamilyAnomalous = 3,
	DeepAnomalous = 4,
}

impl From<Reason> for u64 {
	fn from(reason: Reason) -> Self {
		match reason {
			| Reason::Unverifiable => 1,
			| Reason::Healable => 2,
			| Reason::FamilyAnomalous => 3,
			| Reason::DeepAnomalous => 4,
		}
	}
}

/// Runs the one-time chain cache clear, then the injectivity scan, heal,
/// and repair behind [`MARKER`].
///
/// The clear takes [`CLEAR_MARKER`] and runs ahead of the early return, so
/// a database that already completed the repair still discards its chains.
/// The stamp follows any verdict: a settled repair stamps the empty value,
/// a decline stamps its counters, and only an error leaves the marker
/// unwritten and fails the boot. A heal rescans rather than repairing,
/// because it changes the losers and winners the repair consumes.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn fix(services: &Services) -> Result {
	let global = &services.db["global"];
	let chains_cleared_this_boot = match global.get(CLEAR_MARKER).await {
		| Ok(_) => false,
		| Err(error) if error.is_not_found() => true,
		| Err(error) => return Err(error),
	};

	if chains_cleared_this_boot {
		clear_chain_cache(services).await?;
		services.db["authchainkey_authchain"]
			.sort()
			.log_err()
			.ok();
	}

	match global.get(MARKER).await {
		| Ok(_) => return Ok(()),
		| Err(error) if error.is_not_found() => (),
		| Err(error) => return Err(error),
	}

	for pass in 1..=PASSES {
		let residue = scan(services).await?;

		// The last pass evaluates rather than heals, bounding a residue whose
		// heals never settle.
		if pass < PASSES && heal(services, &residue) {
			continue;
		}

		match repair(services, &residue, chains_cleared_this_boot).await? {
			| Verdict::Settled => global.insert(MARKER, []),
			| Verdict::Declined(reason) => global.raw_put(MARKER, residue.decline_record(reason)),
		}

		break;
	}

	Ok(())
}

/// Discards auth chains cached before walk completeness was enforced.
///
/// A chain truncated at a missing ancestor is well-formed, so no scan
/// separates it from a whole one and the population goes at once. The
/// cache is derived and rebuilds on demand.
#[tracing::instrument(level = "debug", skip_all)]
async fn clear_chain_cache(services: &Services) -> Result {
	let global = &services.db["global"];

	warn!("Discarding cached auth chains; entries from earlier releases may be truncated.");

	clear_chains(services).await?;
	global.insert(CLEAR_MARKER, []);

	Ok(())
}

/// Deletes every auth chain cache row under one cork.
///
/// The fallible scan must finish before its caller finalizes a marker. It is
/// snapshot-based, so it holds only because migrations precede the workers
/// that populate the cache.
pub(super) async fn clear_chains(services: &Services) -> Result {
	let _cork = services.db.cork_and_sync();

	services.db["authchainkey_authchain"]
		.for_clear()
		.ready_try_for_each(|_| Ok(()))
		.await
}

/// Stamps both markers on a fresh database.
///
/// A fresh database never ran the unserialized allocator and holds no
/// cached chains, so it has neither residue to scan for nor a cache to
/// discard.
pub(super) fn mark_clean(services: &Services) {
	let global = &services.db["global"];

	global.insert(MARKER, []);
	global.insert(CLEAR_MARKER, []);
}
