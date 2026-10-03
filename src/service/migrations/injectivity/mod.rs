//! Repair reported short-id residue while preserving unproven data.
//!
//! A versioned record distinguishes a verified clean completion from one that
//! left residue. Legacy markers are kept present so older binaries skip their
//! superseded repair.

mod seen;

use tuwunel_core::result::NotFound;

pub(super) use self::seen::MARKER;
use self::seen::{repair, run, stamp};
use crate::Services;

const LEGACY: [&str; 2] = ["fix_short_injectivity", "clear_auth_chain_cache"];

/// Repairs an eligible database without making repair failures fatal to startup.
///
/// The repair skips read-only opens and records its outcome only once it is
/// durable. Independent migrations continue after an interrupted or unfinished
/// attempt.
pub(super) async fn fix(services: &Services) {
	stamp_legacy(services).await;
	run(&services.db, async || {
		services.server.progress.begin(MARKER);
		repair(services).await
	})
	.await;
}

/// Records a fresh database without scanning its populations.
///
/// Legacy markers retain compatibility with older binaries. The versioned
/// record is written and synced fallibly, so a failed write or sync never
/// certifies it.
pub(super) fn mark_clean(services: &Services) {
	stamp(&services.db);

	let global = &services.db["global"];

	for marker in LEGACY {
		global.insert(marker, []);
	}
}

/// Writes each absent legacy marker so a rollback never reruns the old repair.
///
/// Releases before this repair gate their own scan on the marker's presence
/// alone. Existing values, decline records included, are kept, and a failed
/// read never counts as absence.
async fn stamp_legacy(services: &Services) {
	if services.db.engine.is_read_only() {
		return;
	}

	let global = &services.db["global"];

	for marker in LEGACY {
		if global.get(marker).await.is_missing() {
			global.insert(marker, []);
		}
	}
}
