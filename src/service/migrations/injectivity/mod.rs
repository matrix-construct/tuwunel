//! Repair reported short-id residue while preserving unproven data.
//!
//! A database the superseded repair settled is not eligible. A versioned record
//! distinguishes a verified clean completion from one that left residue. Legacy
//! markers are kept present so older binaries skip their superseded repair.

mod seen;

use tuwunel_core::result::NotFound;
use tuwunel_database::Database;

use self::seen::{MARKER, repair, run, stamp};
use crate::Services;

const SUPERSEDED: &str = "fix_short_injectivity";
const LEGACY: [&str; 2] = [SUPERSEDED, "clear_auth_chain_cache"];

/// Records a fresh database without scanning its populations.
///
/// The versioned record is written and synced fallibly, so a failed write or
/// sync never certifies it. Legacy markers for older binaries follow only a
/// certified record, so a database whose stamp fails stays eligible on the next
/// start.
pub(super) fn mark_clean(services: &Services) {
	if stamp(&services.db).is_none() {
		return;
	}

	let global = &services.db["global"];

	for marker in LEGACY {
		global.insert(marker, []);
	}
}

/// Repairs an eligible database without making repair failures fatal to startup.
///
/// The repair skips a database the superseded repair settled, scanning and
/// writing nothing. It also skips read-only opens and records its outcome only
/// once it is durable. Legacy markers wait for that record, so a database whose
/// attempt ends without one stays eligible on the next start.
pub(super) async fn fix(services: &Services) {
	let db = &services.db;

	if settled(db).await {
		return;
	}

	run(db, async || {
		services.server.progress.begin(MARKER);
		repair(services).await
	})
	.await;

	if db["global"].get(MARKER).await.is_ok() {
		stamp_legacy(services).await;
	}
}

/// Whether the superseded repair settled this database before this one ran.
///
/// The superseded marker's empty value is the only settled form. A decline
/// record, an absent marker or a failed read leaves the database eligible, and
/// a recorded outcome is left for the runner to honor. A build that stamped the
/// legacy markers before attempting this repair leaves the same settled form
/// when its attempt ended early, and nothing tells the two apart.
async fn settled(db: &Database) -> bool {
	let global = &db["global"];

	global.get(MARKER).await.is_missing()
		&& global
			.get(SUPERSEDED)
			.await
			.as_deref()
			.is_ok_and(<[u8]>::is_empty)
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
