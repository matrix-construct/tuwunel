//! Caller contract and result types for a fetch: [`Opts`] in, [`Outcome`] out.
//!
//! [`Op`] selects the federation endpoint and folds into the single-flight
//! dedup key; [`FanoutGrowth`] schedules the staged fan-out width.

use std::num::NonZeroUsize;

use bytes::Bytes;
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedServerName, RoomVersionId,
	api::Direction,
};
use tuwunel_core::smallvec::SmallVec;

use crate::federation::Candidates;

/// Event-id window for the batch ops, inline-sized for the common single-prev
/// case and spilling to the heap past that.
pub type EventWindow = SmallVec<[OwnedEventId; 1]>;

/// Federation endpoint a fetch targets. The dedup key folds this in, so two
/// callers asking for the same event over different endpoints do not coalesce.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Op {
	/// `GET /_matrix/federation/v1/event/{eventId}`
	Event,

	/// `GET /_matrix/federation/v1/event/{eventId}` for an event fetched while
	/// reconstructing an auth chain; routed like [`Op::Event`] but pins the
	/// room's authority server ahead of the popularity ranking.
	AuthEvent,

	/// `GET /_matrix/federation/v1/event_auth/{roomId}/{eventId}`
	AuthChain,

	/// `GET /_matrix/federation/v1/backfill/{roomId}`
	Backfill,

	/// `GET /_matrix/federation/v1/state_ids/{roomId}?event_id=`
	StateIds,

	/// `POST /_matrix/federation/v1/get_missing_events/{roomId}`
	MissingEvents,

	/// `GET /_matrix/federation/v1/timestamp_to_event/{roomId}?ts=&dir=`
	TimestampToEvent,
}

/// Schedules the concurrent candidate width for each staged fan-out round.
///
/// The worker clamps each computed width to its per-round ceiling and remaining
/// attempt budget. `Fixed(1)`, the `Opts::new` default, makes attempts strictly
/// sequential.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanoutGrowth {
	/// Every round races the same width.
	Fixed(NonZeroUsize),

	/// `base`, `base + step`, `base + 2*step`, ...
	Linear {
		base: NonZeroUsize,
		step: NonZeroUsize,
	},

	/// `base`, `base * factor`, `base * factor^2`, ...  Base 1, factor 2 is the
	/// 1 -> 2 -> 4 -> 8 hedging ramp.
	Geometric {
		base: NonZeroUsize,
		factor: NonZeroUsize,
	},
}

impl FanoutGrowth {
	/// Width for round `round` (0-based). Always >= 1; saturating, so a runaway
	/// exponent cannot overflow (the candidate pool and `attempt_limit` clamp
	/// the value to something small regardless).
	#[must_use]
	pub fn round_width(self, round: usize) -> usize {
		match self {
			| Self::Fixed(width) => width.get(),
			| Self::Linear { base, step } => base
				.get()
				.saturating_add(step.get().saturating_mul(round)),
			| Self::Geometric { base, factor } => {
				let exp = u32::try_from(round).unwrap_or(u32::MAX);

				base.get()
					.saturating_mul(factor.get().saturating_pow(exp))
			},
		}
	}
}

/// Caller contract. `event_id` is the sought datum for [`Op::Event`] /
/// [`Op::AuthEvent`] / [`Op::AuthChain`] / [`Op::StateIds`] and a reference
/// point for the others.
#[derive(Clone, Debug)]
pub struct Opts {
	/// Federation endpoint this fetch targets.
	pub op: Op,

	/// Room the fetch is scoped to, or `None` for an unscoped id-addressed
	/// fetch.
	pub room_id: Option<OwnedRoomId>,

	/// Event to fetch (id-addressed ops) or anchor from (room-scoped ops).
	pub event_id: Option<OwnedEventId>,

	/// Timestamp the [`Op::TimestampToEvent`] search starts from; `None` for
	/// every other op.
	pub ts: Option<MilliSecondsSinceUnixEpoch>,

	/// Direction the [`Op::TimestampToEvent`] search runs; `None` for every
	/// other op.
	pub dir: Option<Direction>,

	/// Boundary events the requester already holds; an [`Op::MissingEvents`]
	/// window stops its backward walk here. Empty for every other op.
	pub earliest_events: EventWindow,

	/// Frontier events an [`Op::MissingEvents`] window fills the predecessors
	/// of. Empty for every other op.
	pub latest_events: EventWindow,

	/// Server to try ahead of the ranked candidates.
	pub hint: Option<OwnedServerName>,

	/// Caller-supplied candidate pool tried in place of the room-derived
	/// ranking; empty defers to the room-derived candidates.
	pub candidates: Candidates,

	/// Room version governing id and signature checks; `None` assumes V11.
	pub room_version: Option<RoomVersionId>,

	/// Cap on candidate servers tried; `None` tries every candidate.
	pub attempt_limit: Option<NonZeroUsize>,

	/// Event count requested per [`Op::Backfill`] / [`Op::MissingEvents`] batch
	/// response; defaults to 10.
	pub backfill_limit: Option<NonZeroUsize>,

	/// Per-round width curve for staged fan-out. `Fixed(1)` is sequential.
	pub fanout_growth: FanoutGrowth,

	/// Per-round concurrency ceiling. `None` lets the curve run free, clamped
	/// only by the candidate pool and `attempt_limit`; `Some(n)` caps each
	/// round at `n`.
	pub fanout_max_width: Option<NonZeroUsize>,

	/// Cap on escalation rounds before giving up. `None` runs until exhaustion.
	pub fanout_rounds: Option<NonZeroUsize>,

	/// Reject a response whose event does not hash to the requested id.
	pub check_event_id: bool,

	/// Reject a response that is not well-formed JSON.
	pub check_conforms: bool,

	/// Reject a response that fails content-hash verification.
	pub check_hashes: bool,

	/// Accepted but not yet consulted; redaction-aware hash verification is
	/// unimplemented.
	pub authoritative_redaction: bool,

	/// Reject a response that fails signature verification.
	pub check_signature: bool,
}

impl Opts {
	/// Scope a fetch to a room.
	#[must_use]
	pub fn new(op: Op, room_id: OwnedRoomId) -> Self { Self::with_room_id(op, Some(room_id)) }

	/// A fetch with no room scope, for id-addressed callers such as
	/// `get-remote-pdu`; room-derived candidate ranking is skipped, leaving the
	/// hint, the caller-supplied pool, and the event id's origin.
	#[must_use]
	pub fn unscoped(op: Op) -> Self { Self::with_room_id(op, None) }

	/// All validation toggles default on; the caller relaxes them per request.
	fn with_room_id(op: Op, room_id: Option<OwnedRoomId>) -> Self {
		Self {
			op,
			room_id,
			event_id: None,
			ts: None,
			dir: None,
			earliest_events: EventWindow::new(),
			latest_events: EventWindow::new(),
			hint: None,
			candidates: Candidates::new(),
			room_version: None,
			attempt_limit: None,
			backfill_limit: None,
			fanout_growth: FanoutGrowth::Fixed(NonZeroUsize::MIN),
			fanout_max_width: None,
			fanout_rounds: None,
			check_event_id: true,
			check_conforms: true,
			check_hashes: true,
			authoritative_redaction: true,
			check_signature: true,
		}
	}

	/// Set the target event; required for the id-addressed ops.
	#[must_use]
	pub fn event_id(self, event_id: OwnedEventId) -> Self {
		Self { event_id: Some(event_id), ..self }
	}

	/// Set the timestamp the [`Op::TimestampToEvent`] search starts from.
	#[must_use]
	pub fn ts(self, ts: MilliSecondsSinceUnixEpoch) -> Self { Self { ts: Some(ts), ..self } }

	/// Set the direction the [`Op::TimestampToEvent`] search runs.
	#[must_use]
	pub fn dir(self, dir: Direction) -> Self { Self { dir: Some(dir), ..self } }

	/// Set the boundary the [`Op::MissingEvents`] backward walk stops at.
	#[must_use]
	pub fn earliest_events<I>(self, earliest_events: I) -> Self
	where
		I: IntoIterator<Item = OwnedEventId>,
	{
		Self {
			earliest_events: earliest_events.into_iter().collect(),
			..self
		}
	}

	/// Set the frontier an [`Op::MissingEvents`] window fills behind.
	#[must_use]
	pub fn latest_events<I>(self, latest_events: I) -> Self
	where
		I: IntoIterator<Item = OwnedEventId>,
	{
		Self {
			latest_events: latest_events.into_iter().collect(),
			..self
		}
	}

	/// Try the named server ahead of the ranked candidates.
	#[must_use]
	pub fn hint(self, hint: OwnedServerName) -> Self { Self { hint: Some(hint), ..self } }

	/// Supply the candidate pool verbatim, bypassing the room-derived ranking.
	#[must_use]
	pub fn candidates<I>(self, candidates: I) -> Self
	where
		I: IntoIterator<Item = OwnedServerName>,
	{
		Self {
			candidates: candidates.into_iter().collect(),
			..self
		}
	}

	/// Room version for [`Op::Event`] id and signature checks. `None` keeps the
	/// V11 default, so callers on a non-V11 room must name it to avoid a
	/// spurious rejection.
	#[must_use]
	pub fn room_version(self, room_version: RoomVersionId) -> Self {
		Self { room_version: Some(room_version), ..self }
	}

	/// Cap the number of candidate servers tried.
	#[must_use]
	pub fn attempt_limit(self, attempt_limit: NonZeroUsize) -> Self {
		Self {
			attempt_limit: Some(attempt_limit),
			..self
		}
	}

	/// Set the events requested per [`Op::Backfill`] / [`Op::MissingEvents`]
	/// batch.
	#[must_use]
	pub fn backfill_limit(self, backfill_limit: NonZeroUsize) -> Self {
		Self {
			backfill_limit: Some(backfill_limit),
			..self
		}
	}

	/// Set the per-round fan-out width schedule.
	#[must_use]
	pub fn fanout(self, growth: FanoutGrowth) -> Self { Self { fanout_growth: growth, ..self } }

	/// Cap the per-round fan-out concurrency.
	#[must_use]
	pub fn fanout_max_width(self, max_width: NonZeroUsize) -> Self {
		Self {
			fanout_max_width: Some(max_width),
			..self
		}
	}

	/// Cap the number of escalation rounds.
	#[must_use]
	pub fn fanout_rounds(self, rounds: NonZeroUsize) -> Self {
		Self { fanout_rounds: Some(rounds), ..self }
	}

	/// Apply the op's advised staged-fan-out ramp. `Opts::new` is otherwise
	/// dark on every op, so a callsite opts in by chaining this; the generic
	/// and single-shot-batch ops keep the sequential default.
	#[must_use]
	pub fn fanout_for_op(self) -> Self {
		use FanoutGrowth::{Geometric, Linear};

		const ONE: NonZeroUsize = NonZeroUsize::new(1).unwrap();
		const TWO: NonZeroUsize = NonZeroUsize::new(2).unwrap();
		const THREE: NonZeroUsize = NonZeroUsize::new(3).unwrap();
		const FOUR: NonZeroUsize = NonZeroUsize::new(4).unwrap();
		const FIVE: NonZeroUsize = NonZeroUsize::new(5).unwrap();

		match self.op {
			| Op::AuthEvent => self
				.fanout(Geometric { base: ONE, factor: TWO })
				.fanout_max_width(FOUR)
				.fanout_rounds(FIVE),
			| Op::AuthChain => self
				.fanout(Linear { base: ONE, step: ONE })
				.fanout_max_width(TWO)
				.fanout_rounds(TWO),
			| Op::StateIds => self
				.fanout(Linear { base: ONE, step: ONE })
				.fanout_max_width(THREE)
				.fanout_rounds(THREE),
			| Op::MissingEvents => self
				.fanout(Geometric { base: ONE, factor: TWO })
				.fanout_rounds(THREE),
			| Op::Event | Op::Backfill | Op::TimestampToEvent => self,
		}
	}

	/// Toggle every validation gate at once. Callers that re-validate
	/// downstream pass `false` to fetch raw bytes without rejecting non-V11
	/// events.
	#[must_use]
	pub fn checks(self, enabled: bool) -> Self {
		Self {
			check_event_id: enabled,
			check_conforms: enabled,
			check_hashes: enabled,
			check_signature: enabled,
			..self
		}
	}
}

/// Raw response body plus the server that answered. `bytes` is ref-counted so
/// concurrent callers coalesced onto one fetch share a single buffer.
#[derive(Debug)]
pub struct Outcome {
	pub bytes: Bytes,
	pub origin: OwnedServerName,
}
