//! Short id injectivity: the one-time scan and repair.
//!
//! Releases before v1.8.3 could mint two short ids for one identity,
//! leaving stale reverse rows in both families, ghost entries in a few
//! compressed states, and auth chains cached from both allocations. The
//! migration measures that residue, repairs what matches the shapes it
//! handles, and marks itself complete like any other.

mod repair;
mod scan;

use tuwunel_core::{Result, result::NotFound};

use self::{repair::repair, scan::scan};
use crate::Services;

/// Global marker recording the repair ran to completion.
///
/// Refused or unverifiable residue leaves it unwritten, so the next boot
/// scans again.
static MARKER: &[u8] = b"fix_short_injectivity";

/// Runs the injectivity scan and repair once, behind [`MARKER`].
///
/// The stamp follows the repair's own verdict: only a settled repair
/// writes it.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn fix(services: &Services) -> Result {
	let global = &services.db["global"];

	if global.get(MARKER).await.is_not_found() {
		let residue = scan(services).await?;

		if repair(services, &residue).await? {
			global.insert(MARKER, []);
		}
	}

	Ok(())
}

/// Stamps the marker on a fresh database.
///
/// A fresh database never ran the unserialized allocator, so there is no
/// residue to scan for.
pub(super) fn mark_clean(services: &Services) { services.db["global"].insert(MARKER, []); }
