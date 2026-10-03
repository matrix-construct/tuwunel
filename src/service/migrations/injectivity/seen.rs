use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Result, async_noinline, err, implement, info,
	result::NotFound,
	smallvec::SmallVec,
	utils::{TryReadyExt, u64_from_bytes},
	warn,
};
use tuwunel_database::{Database, Json, Txn, keyval::ValBuf, serialize_key, serialize_val};

use self::{
	identity::{Family, Kind, census as census_identities, cleanup, repair as repair_identities},
	references::References,
	rooms::{inspect as inspect_rooms, repair as repair_rooms},
	state::{
		gc::{collect, inspect as inspect_gc},
		inspect as inspect_states, repair as repair_states,
	},
};
use crate::{Services, migrations::scan::ScanExt};

mod identity;
mod references;
mod rooms;
mod state;
#[cfg(test)]
mod tests;

type Samples = SmallVec<[Sample; 1]>;
type Identity = SmallVec<[u8; 48]>;

pub(super) const MARKER: &str = "repair_short_injectivity_seen";
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

	/// Each count tallies occurrences, including overlaps and unknown censuses.
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

#[derive(Clone, Copy, Default)]
struct Uncertain {
	states: bool,
	collected: bool,
	rooms: bool,
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

/// Completes independent repair passes and verifies their residual populations.
///
/// Fresh identity and reference censuses precede final alias reclamation. Every
/// final inspection is read-only and uses committed raw rows.
// query-depth firewall
#[async_noinline]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn repair(services: &Services) -> Result<Outcome> {
	let progress = &services.server.progress;

	progress.enter("short ids");
	let identities = repair_identities(services).await?;

	progress.enter("state snapshots");
	let states = repair_states(services, &identities).await?;

	progress.enter("unreachable states");
	let collected = collect(services).await?;

	progress.enter("rooms");
	let rooms = repair_rooms(services).await?;

	progress.enter("references");
	let identities = census_identities(services).await?;
	let references = References::census(services, &identities).await?;

	cleanup(services, &identities.events, &references.events, references.event_complete).await?;

	cleanup(
		services,
		&identities.statekeys,
		&references.statekeys,
		references.statekey_complete,
	)
	.await?;

	// A verified restore leaves its own unit unestablished; independent stages still ran.
	if states.restored || rooms.restored {
		return Err!(
			"published data failed verification and was restored; completion remains \
			 unestablished"
		);
	}

	progress.enter("verify");
	services.server.check_running()?;
	services.db.engine.sync()?;

	let uncertain = Uncertain {
		states: states.uncertain,
		collected: collected.unknown,
		rooms: rooms.unknown,
	};

	let outcome = verify(services, uncertain).await?;

	info!(
		rewritten = states.rewritten,
		original_bytes = states.original_bytes,
		snapshot_bytes = states.snapshot_bytes,
		collected = collected.deleted,
		purged = rooms.deleted,
		moved = rooms.moved,
		?outcome.counts,
		"Injectivity repair verified; purge residue counts include unclassified room strays."
	);

	Ok(outcome)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn verify(services: &Services, uncertain: Uncertain) -> Result<Outcome> {
	services.server.check_running()?;
	let identities = census_identities(services).await?;
	let outcome = Outcome::clean()
		.residual_family(&identities.events, Shape::EventAlias)?
		.residual_family(&identities.statekeys, Shape::StatekeyAlias)?;

	let outcome = inspect_states(services, &identities, outcome).await?;
	let collected = inspect_gc(services).await?;
	let rooms = inspect_rooms(services).await?;

	outcome
		.residues(Shape::UnreachableState, &collected.unfinished, Reason::References)?
		// An unclassified stray counts once as purge residue, even as a live-room alias.
		.residues(Shape::PurgeResidue, &rooms.unfinished, Reason::Ownership)?
		.uncertain(Shape::AffectedState, uncertain.states, Reason::HistoricalState)?
		.uncertain(
			Shape::UnreachableState,
			uncertain.collected || collected.unknown,
			Reason::References,
		)?
		.uncertain(Shape::PurgeResidue, uncertain.rooms || rooms.unknown, Reason::Ownership)
}

#[implement(Outcome)]
fn residual_family(self, family: &Family, alias: Shape) -> Result<Self> {
	family
		.candidates
		.iter()
		.try_fold(self, |outcome, candidate| {
			let shape = candidate_shape(candidate.kind, alias);

			outcome.residue(shape, 1, &candidate.identity, Reason::Identity)
		})?
		.residue(alias, family.malformed, &[], Reason::Identity)
}

fn candidate_shape(kind: Kind, alias: Shape) -> Shape {
	match kind {
		| Kind::Reverse => Shape::DanglingWinner,
		| Kind::Forward => Shape::PromotableReverse,
		| Kind::Alias(_) | Kind::Unresolved => alias,
	}
}

#[implement(Outcome)]
fn residues(self, shape: Shape, ids: &BTreeSet<u64>, reason: Reason) -> Result<Self> {
	ids.iter()
		.try_fold(self, |outcome, id| outcome.residue(shape, 1, &id.to_be_bytes(), reason))
}

#[implement(Outcome)]
fn uncertain(self, shape: Shape, unknown: bool, reason: Reason) -> Result<Self> {
	self.residue(shape, u64::from(unknown), &[], reason)
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
	if outcome.status == Status::Unfinished {
		warn!(
			?outcome.counts,
			?outcome.samples,
			truncated = outcome.truncated,
			"Injectivity repair left unrepaired rows; purge residue counts include \
			 unclassified room strays."
		);
	}

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
fn residue(mut self, shape: Shape, count: u64, identity: &[u8], reason: Reason) -> Result<Self> {
	if count == 0 {
		return Ok(self);
	}

	let total = shape
		.slot()
		.and_then(|slot| self.counts.get_mut(slot))
		.ok_or_else(|| err!("unsupported injectivity diagnostic shape"))?;

	*total = total.saturating_add(count);
	self.status = Status::Unfinished;
	if self.samples.len() < SAMPLE_LIMIT {
		let identity = identity.get(..IDENTITY_LIMIT).unwrap_or(identity);

		self.samples.push(Sample {
			shape,
			identity: Identity::from_slice(identity),
			reason,
		});
	} else {
		self.truncated = true;
	}

	self.truncated |= identity.len() > IDENTITY_LIMIT || count > 1;
	Ok(self)
}

impl Shape {
	fn slot(self) -> Option<usize> { SHAPES.iter().position(|shape| *shape == self) }
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

fn short_of(bytes: &[u8]) -> Option<u64> { u64_from_bytes(bytes).ok() }
