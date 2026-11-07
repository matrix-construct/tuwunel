//! The short-id census as a stream of findings.
//!
//! The repair's final verification folds this stream into the outcome it
//! records, so a run reports what verification would record for the database as
//! it stands. Nothing here writes.

use std::fmt;

use futures::{Stream, StreamExt, TryFutureExt, TryStreamExt, future::ready};
use tuwunel_core::{Result, implement, utils::IterStream};

use super::{
	Identity, Reason, Shape,
	gc::{Collected, inspect as inspect_gc},
	identity::{Family, Identities, Kind, census as census_identities},
	rooms::{Rooms, inspect as inspect_rooms},
	state::{findings as state_findings, survey},
};
use crate::{Services, integrity::Report};

/// Residue the short-id census finds, in the terms the repair's outcome records.
///
/// The outcome counts a finding once under its shape, or a `Malformed` subject's
/// number of times, and keeps the first few as samples.
pub(crate) struct Finding {
	/// The class of residue, which names the outcome slot it counts in.
	pub(crate) shape: Shape,

	/// Where the census found it.
	pub(crate) subject: Subject,

	/// What kept the repair from resolving it.
	pub(crate) reason: Reason,
}

/// Where a finding lies.
///
/// Most subjects name one row, id or state, and verification samples its bytes.
/// The rest stand for findings no single row carries: a total, an incomplete
/// census, or residue a census could not rule out.
pub(crate) enum Subject {
	/// An identity row, by the event id or state key it names.
	Identity(Identity),

	/// A short id a state entry names.
	Short(u64),

	/// A state, by its short state hash.
	State(u64),

	/// A short room id.
	Room(u64),

	/// A family's malformed identity rows, as one total.
	///
	/// Each of those rows also comes as its own `Identity` finding, and the
	/// outcome records both, so only verification takes this one.
	Malformed(u64),

	/// A state census that could not attribute every row.
	Incomplete,

	/// A census that could not prove further residue absent.
	///
	/// It comes at the end of the stream, where verification joins it with the
	/// uncertainty the repair captured before its own writes.
	Uncertain,
}

/// Streams the short-id residue, one finding per defect.
///
/// This is the census verification folds, less each family's malformed row
/// total, which repeats rows already streamed one by one.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn check(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	scan(services).try_filter(|finding| ready(!matches!(finding.subject, Subject::Malformed(_))))
}

/// Streams the short-id residue the repair's verification records.
///
/// Findings come in the order the outcome records them: identity rows, states,
/// unreachable states and short room ids, then the censuses that could not prove
/// further residue absent. Each describes the database as it stands, and none
/// forecasts a repair: publishing snapshots drops digest roots and parent links,
/// so a repair can collect unreachable states that a run before it does not
/// report.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	let states = census_identities(services)
		.and_then(async |identities| {
			let census = survey(services, &identities).await?;
			let findings = identity_findings(identities)
				.try_stream()
				.chain(state_findings(services, census));

			Ok(findings)
		})
		.try_flatten_stream();

	let residues = inspect_gc(services)
		.and_then(async |collected| {
			let rooms = inspect_rooms(services).await?;
			let findings = residue_findings(collected, rooms).try_stream();

			Ok(findings)
		})
		.try_flatten_stream();

	states.chain(residues)
}

impl Report for Finding {
	#[inline]
	fn kind(&self) -> &'static str { self.shape.label() }
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}: {} ({})", self.kind(), self.subject, self.reason.label())
	}
}

impl fmt::Display for Subject {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Identity(identity) => write!(f, "identity {}", identity.escape_ascii()),
			| Self::Short(short) => write!(f, "short id {short}"),
			| Self::State(state) => write!(f, "state {state}"),
			| Self::Room(room) => write!(f, "short room id {room}"),
			| Self::Malformed(count) => write!(f, "{count} malformed identity rows"),
			| Self::Incomplete => write!(f, "a state census that could not attribute every row"),
			| Self::Uncertain =>
				write!(f, "an incomplete census leaves further residue unproven"),
		}
	}
}

fn identity_findings(
	Identities { events, statekeys }: Identities,
) -> impl Iterator<Item = Finding> + Send {
	family(events, Shape::EventAlias).chain(family(statekeys, Shape::StatekeyAlias))
}

fn family(family: Family, alias: Shape) -> impl Iterator<Item = Finding> + Send {
	let Family { candidates, malformed, .. } = family;
	let unnamed = malformed.ne(&0).then_some(Finding {
		shape: alias,
		subject: Subject::Malformed(malformed),
		reason: Reason::Identity,
	});

	candidates
		.into_iter()
		.map(move |candidate| Finding {
			shape: candidate_shape(candidate.kind, alias),
			subject: Subject::Identity(candidate.identity),
			reason: Reason::Identity,
		})
		.chain(unnamed)
}

fn candidate_shape(kind: Kind, alias: Shape) -> Shape {
	match kind {
		| Kind::Reverse => Shape::DanglingWinner,
		| Kind::Forward => Shape::PromotableReverse,
		| Kind::Alias(_) | Kind::Unresolved => alias,
	}
}

fn residue_findings(collected: Collected, rooms: Rooms) -> impl Iterator<Item = Finding> + Send {
	let unreachable = collected
		.unfinished
		.into_iter()
		.map(|id| Finding {
			shape: Shape::UnreachableState,
			subject: Subject::State(id),
			reason: Reason::References,
		});

	// An unclassified stray counts once as purge residue, even as a live-room alias.
	let purged = rooms.unfinished.into_iter().map(|id| Finding {
		shape: Shape::PurgeResidue,
		subject: Subject::Room(id),
		reason: Reason::Ownership,
	});

	let uncertain = [
		collected
			.unknown
			.then_some((Shape::UnreachableState, Reason::References)),
		rooms
			.unknown
			.then_some((Shape::PurgeResidue, Reason::Ownership)),
	];

	let uncertain = uncertain
		.into_iter()
		.flatten()
		.map(|(shape, reason)| Finding {
			shape,
			subject: Subject::Uncertain,
			reason,
		});

	unreachable.chain(purged).chain(uncertain)
}

#[implement(Shape)]
fn label(self) -> &'static str {
	match self {
		| Self::EventAlias => "event-short-id",
		| Self::StatekeyAlias => "statekey-short-id",
		| Self::DanglingWinner => "dangling-winner",
		| Self::PromotableReverse => "promotable-reverse",
		| Self::EventAliasEntry => "event-alias-entry",
		| Self::StatekeyAliasEntry => "statekey-alias-entry",
		| Self::AffectedState => "affected-state",
		| Self::DiffCollision => "diff-collision",
		| Self::EventOrphanEntry => "event-orphan-entry",
		| Self::StatekeyOrphanEntry => "statekey-orphan-entry",
		| Self::AuthChainCache => "auth-chain-cache",
		| Self::UnreachableState => "unreachable-state",
		| Self::PurgeResidue => "purge-residue",
		| Self::LiveRoomAlias => "live-room-alias",
		| Self::DeclinedMarker => "declined-marker",
	}
}

#[implement(Reason)]
fn label(self) -> &'static str {
	match self {
		| Self::Identity => "identity unproven",
		| Self::HistoricalState => "history unproven",
		| Self::References => "references unproven",
		| Self::Ownership => "ownership unproven",
	}
}
