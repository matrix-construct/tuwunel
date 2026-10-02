use serde::{Deserialize, Serialize};
use tuwunel_core::{Err, Result, implement, result::NotFound, smallvec::SmallVec, warn};
use tuwunel_database::{Database, Json, Txn, keyval::ValBuf, serialize_key, serialize_val};

mod identity;
mod references;
#[cfg(test)]
mod tests;

type Samples = SmallVec<[Sample; 1]>;
type Identity = SmallVec<[u8; 48]>;

const MARKER: &str = "repair_short_injectivity_seen";
const VERSION: u8 = 1;
const SAMPLE_LIMIT: usize = 4;
const IDENTITY_LIMIT: usize = 128;
const RECORD_LIMIT: usize = 4096;

// Stored count positions, so the order is part of the record format.
const SHAPES: [Shape; 15] = [
	Shape::EventAlias,
	Shape::StatekeyAlias,
	Shape::DanglingWinner,
	Shape::PromotableReverse,
	Shape::EventAliasEntry,
	Shape::StatekeyAliasEntry,
	Shape::AffectedState,
	Shape::DiffCollision,
	Shape::EventOrphanEntry,
	Shape::StatekeyOrphanEntry,
	Shape::AuthChainCache,
	Shape::UnreachableState,
	Shape::PurgeResidue,
	Shape::LiveRoomAlias,
	Shape::DeclinedMarker,
];

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Outcome {
	version: u8,
	status: Status,
	counts: [u64; SHAPES.len()],
	samples: Samples,
	truncated: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
	Clean,
	Unfinished,
}

/// Residue a completed repair leaves behind, as its outcome records it.
///
/// Each discriminant is the code a stored sample carries. The position in
/// `SHAPES` is the slot its count occupies.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(into = "u8", try_from = "u8")]
enum Shape {
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Sample {
	shape: Shape,
	identity: Identity,
	reason: Reason,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Reason {
	Identity,
	HistoricalState,
	References,
	Ownership,
}

struct Boundary<'a> {
	db: &'a Database,
}

impl From<Shape> for u8 {
	fn from(shape: Shape) -> Self {
		match shape {
			| Shape::EventAlias => 1,
			| Shape::StatekeyAlias => 2,
			| Shape::DanglingWinner => 3,
			| Shape::PromotableReverse => 4,
			| Shape::EventAliasEntry => 12,
			| Shape::StatekeyAliasEntry => 13,
			| Shape::AffectedState => 15,
			| Shape::DiffCollision => 19,
			| Shape::EventOrphanEntry => 23,
			| Shape::StatekeyOrphanEntry => 24,
			| Shape::AuthChainCache => 27,
			| Shape::UnreachableState => 29,
			| Shape::PurgeResidue => 32,
			| Shape::LiveRoomAlias => 33,
			| Shape::DeclinedMarker => 37,
		}
	}
}

impl TryFrom<u8> for Shape {
	type Error = u8;

	fn try_from(code: u8) -> Result<Self, Self::Error> {
		match code {
			| 1 => Ok(Self::EventAlias),
			| 2 => Ok(Self::StatekeyAlias),
			| 3 => Ok(Self::DanglingWinner),
			| 4 => Ok(Self::PromotableReverse),
			| 12 => Ok(Self::EventAliasEntry),
			| 13 => Ok(Self::StatekeyAliasEntry),
			| 15 => Ok(Self::AffectedState),
			| 19 => Ok(Self::DiffCollision),
			| 23 => Ok(Self::EventOrphanEntry),
			| 24 => Ok(Self::StatekeyOrphanEntry),
			| 27 => Ok(Self::AuthChainCache),
			| 29 => Ok(Self::UnreachableState),
			| 32 => Ok(Self::PurgeResidue),
			| 33 => Ok(Self::LiveRoomAlias),
			| 37 => Ok(Self::DeclinedMarker),
			| unknown => Err(unknown),
		}
	}
}

/// Runs the repair unless a recognized outcome is already recorded.
///
/// The repair must finish its durability barriers and uncached verification
/// before returning, and its outcome is recorded only after another sync. A
/// read-only open or any error leaves completion unestablished without failing
/// startup.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn run(
	db: &Database,
	repair: impl AsyncFnOnce() -> Result<Outcome>,
) -> Option<Outcome> {
	Boundary::writable(db)?
		.attempt(repair)
		.await
		.inspect_err(|error| {
			warn!(%error, "Injectivity repair interrupted; completion remains unestablished.");
		})
		.ok()
		.flatten()
}

/// Records a clean outcome for a fresh database without scanning it.
///
/// The record is written and synced before it counts. A read-only open or a
/// failed write or sync returns `None` and leaves completion unestablished.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) fn stamp(db: &Database) -> Option<Outcome> {
	Boundary::writable(db)?
		.stamp()
		.inspect_err(|error| {
			warn!(%error, "Injectivity clean stamp failed; completion remains unestablished.");
		})
		.ok()
}

#[implement(Boundary, generics = "<'a>", params = "<'a>")]
fn writable(db: &'a Database) -> Option<Self> {
	if db.engine.is_read_only() {
		warn!("Injectivity repair cannot run on a read-only or secondary database.");
		return None;
	}

	Some(Self { db })
}

#[implement(Boundary, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn attempt(
	&self,
	repair: impl AsyncFnOnce() -> Result<Outcome>,
) -> Result<Option<Outcome>> {
	let db = self.db;
	let recorded = db["global"]
		.get(MARKER)
		.await
		.present()?
		.as_deref()
		.and_then(Outcome::decode);

	if let Some(outcome) = recorded {
		db.engine.sync()?;
		if outcome.status == Status::Unfinished {
			warn!(
				?outcome.counts,
				"Injectivity repair left unrepaired rows on an earlier start."
			);
		}

		return Ok(None);
	}

	let outcome = repair().await?;
	let bytes = outcome.encode()?;

	db.engine.sync()?;
	self.record(bytes)?;

	Ok(Some(outcome))
}

#[implement(Boundary, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
fn stamp(&self) -> Result<Outcome> {
	let outcome = Outcome::clean();

	self.record(outcome.encode()?)?;

	Ok(outcome)
}

#[implement(Boundary, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
fn record(&self, bytes: ValBuf) -> Result {
	let key = serialize_key((MARKER,))?;

	Txn::insert(&self.db["global"], [(key, bytes)])
		.try_execute()
		.map_err(Into::into)
}

#[implement(Outcome)]
fn clean() -> Self {
	Self {
		version: VERSION,
		status: Status::Clean,
		counts: [0; SHAPES.len()],
		samples: Samples::new(),
		truncated: false,
	}
}

#[implement(Outcome)]
fn encode(&self) -> Result<ValBuf> {
	if !self.valid() {
		return Err!("invalid injectivity repair outcome");
	}

	serialize_val(Json(self))
}

#[implement(Outcome)]
fn decode(bytes: &[u8]) -> Option<Self> {
	Some(bytes)
		.filter(|bytes| bytes.len() <= RECORD_LIMIT)
		.and_then(|bytes| serde_json::from_slice(bytes).ok())
		.filter(Self::valid)
}

#[implement(Outcome)]
fn valid(&self) -> bool {
	self.version == VERSION
		&& self.samples.len() <= SAMPLE_LIMIT
		&& self
			.samples
			.iter()
			.all(|sample| sample.identity.len() <= IDENTITY_LIMIT)
		&& match self.status {
			| Status::Unfinished => self.counts.iter().any(|count| *count > 0),
			| Status::Clean =>
				self.counts == [0; SHAPES.len()] && self.samples.is_empty() && !self.truncated,
		}
}
