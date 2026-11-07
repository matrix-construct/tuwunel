//! Short event and state key ids, and the residue their damage leaves.
//!
//! The injectivity repair's final verification folds the same census into the
//! outcome it records, so a run reports what verification would record for the
//! database as it stands, each defect once. It compares columns, so it runs only
//! where nothing else writes.

pub(crate) mod check;
pub(crate) mod gc;
pub(crate) mod identity;
pub(crate) mod references;
pub(crate) mod rooms;
pub(crate) mod state;

use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Result,
	smallvec::SmallVec,
	utils::{TryReadyExt, u64_from_bytes},
};

use self::check::check;
use super::{Check, ScanExt, tally};
use crate::Services;

pub(crate) type Identity = SmallVec<[u8; 48]>;

pub(super) static CHECK: Check = Check {
	name: "short-ids",
	about: "Short event and state key ids pair one to one, every state resolves cleanly onto \
	        sound ones, and no unreachable state or unowned short room id remains",
	quiescent: true,
	walk: |services, limit| tally(check(services), limit),
};

/// Residue a completed repair leaves behind, as its outcome records it.
///
/// Each discriminant is the code a stored sample carries. The position in
/// `SHAPES` is the slot its count occupies.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(into = "u8", try_from = "u8")]
pub(crate) enum Shape {
	/// Alias, ambiguous or malformed event short-id rows.
	EventAlias = 1,

	/// Alias, ambiguous or malformed state-key short-id rows.
	StatekeyAlias = 2,

	/// A forward row whose short has no reverse row.
	DanglingWinner = 3,

	/// A reverse row whose identity has no forward row.
	PromotableReverse = 4,

	/// A state entry naming an alias or refused event short.
	EventAliasEntry = 12,

	/// A state entry naming an alias or refused state-key short.
	StatekeyAliasEntry = 13,

	/// A state whose history could not be verified or rebuilt.
	AffectedState = 15,

	/// A state diff that adds and removes the same entry.
	DiffCollision = 19,

	/// A state entry naming an event short without a reverse row.
	EventOrphanEntry = 23,

	/// A state entry naming a state-key short without a reverse row.
	StatekeyOrphanEntry = 24,

	/// A cached auth chain derived from a damaged short mapping.
	AuthChainCache = 27,

	/// An unreachable state the collector could not remove.
	UnreachableState = 29,

	/// A short room id left behind by a purged room.
	PurgeResidue = 32,

	/// A second short room id of a live room.
	LiveRoomAlias = 33,

	/// A legacy marker that declined the repair.
	DeclinedMarker = 37,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
	Identity,
	HistoricalState,
	References,
	Ownership,
}

pub(crate) fn short_of(bytes: &[u8]) -> Option<u64> { u64_from_bytes(bytes).ok() }

#[tracing::instrument(level = "debug", skip(services, init, step))]
async fn sweep<T: Send>(
	services: &Services,
	column: &str,
	init: T,
	step: impl Fn(T, &[u8], &[u8]) -> T + Sync,
) -> Result<T> {
	services.db[column]
		.raw_stream()
		.scanned(&services.server)
		.ready_try_fold(init, |acc, (key, value)| Ok(step(acc, key, value)))
		.await
}
