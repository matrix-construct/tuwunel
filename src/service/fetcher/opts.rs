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

/// Stores an event-ID window for batch federation operations.
///
/// One entry remains inline for the common single-previous-event case, while
/// larger windows spill to the heap.
pub type EventWindow = SmallVec<[OwnedEventId; 1]>;

/// Identifies the federation endpoint targeted by a fetch.
///
/// The operation participates in the dedup key, so callers using different
/// endpoints never coalesce even when their other options match.
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
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FanoutGrowth {
	/// Every round races the same width.
	Fixed(NonZeroUsize),

	/// `base`, `base + step`, `base + 2*step`, ...
	Linear {
		/// Width used for the first round.
		base: NonZeroUsize,

		/// Width added for each subsequent round.
		step: NonZeroUsize,
	},

	/// `base`, `base * factor`, `base * factor^2`, ...  Base 1, factor 2 is the
	/// 1 -> 2 -> 4 -> 8 hedging ramp.
	Geometric {
		/// Width used for the first round.
		base: NonZeroUsize,

		/// Multiplier applied for each subsequent round.
		factor: NonZeroUsize,
	},
}

impl FanoutGrowth {
	/// Computes the candidate width for a zero-based fanout round.
	///
	/// The arithmetic saturates before the worker clamps the result to the
	/// candidate pool, per-round ceiling, and remaining attempt budget.
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

/// Describes one caller's federation fetch policy.
///
/// `event_id` addresses Event, AuthEvent, AuthChain, and StateIds requests and
/// anchors Backfill; MissingEvents uses its event windows, while
/// TimestampToEvent uses `ts` and `dir`. Candidate, retry, fanout, and
/// validation settings also participate in single-flight identity.
#[derive(Clone, Debug)]
pub struct Opts {
	/// Federation endpoint this fetch targets.
	pub op: Op,

	/// Room the fetch is scoped to, or `None` for an unscoped id-addressed
	/// fetch.
	pub room_id: Option<OwnedRoomId>,

	/// Target for Event, AuthEvent, AuthChain, and StateIds or Backfill anchor; unused by MissingEvents and TimestampToEvent.
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

	/// Room version for Event and AuthEvent validation; `None` assumes V11, and other operations do not consult it.
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

	/// Optional per-call round cap; the global round cap still applies.
	pub fanout_rounds: Option<NonZeroUsize>,

	/// For Event and AuthEvent, reject a response whose calculated event ID differs; other operations ignore this flag.
	pub check_event_id: bool,

	/// Reject a response that is not well-formed JSON.
	pub check_conforms: bool,

	/// Requests combined event verification for Event and AuthEvent responses.
	///
	/// Either this flag or `check_signature` runs the verifier. Its returned
	/// `Verified` status is currently ignored, so a content-hash mismatch with
	/// valid signatures is accepted; other operations ignore the flag.
	pub check_hashes: bool,

	/// Accepted but not yet consulted; redaction-aware hash verification is
	/// unimplemented.
	pub authoritative_redaction: bool,

	/// For Event and AuthEvent, reject verifier errors when either verification flag is enabled; other operations ignore this flag.
	pub check_signature: bool,
}

impl Opts {
	/// Creates a fetch scoped to a room.
	///
	/// Validation gates start enabled, while the fixed width of one keeps
	/// attempts sequential unless the caller opts into staged fanout.
	#[must_use]
	pub fn new(op: Op, room_id: OwnedRoomId) -> Self { Self::with_room_id(op, Some(room_id)) }

	/// Creates an unscoped fetch for an ID-addressed operation.
	///
	/// Room-derived candidate ranking is skipped, leaving the hint,
	/// caller-supplied pool, and event ID origin as candidate sources.
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

	/// Sets the target event or Backfill anchor.
	///
	/// Event, AuthEvent, AuthChain, Backfill, and StateIds require this value;
	/// MissingEvents and TimestampToEvent do not consult it.
	#[must_use]
	pub fn event_id(self, event_id: OwnedEventId) -> Self {
		Self { event_id: Some(event_id), ..self }
	}

	/// Sets the timestamp for a [`Op::TimestampToEvent`] search.
	///
	/// Other operations retain the value in coalescing identity but do not
	/// consult it during transport.
	#[must_use]
	pub fn ts(self, ts: MilliSecondsSinceUnixEpoch) -> Self { Self { ts: Some(ts), ..self } }

	/// Sets the direction for a [`Op::TimestampToEvent`] search.
	///
	/// Other operations retain the value in coalescing identity but do not
	/// consult it during transport.
	#[must_use]
	pub fn dir(self, dir: Direction) -> Self { Self { dir: Some(dir), ..self } }

	/// Sets the boundary where a [`Op::MissingEvents`] backward walk stops.
	///
	/// Only MissingEvents consults this window, whose order is normalized in the
	/// single-flight key.
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

	/// Sets the frontier a [`Op::MissingEvents`] request fills behind.
	///
	/// Only MissingEvents consults this window, whose order is normalized in the
	/// single-flight key.
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

	/// Adds a server ahead of the initial candidate order.
	///
	/// Reachability ranking may still drop or deprioritize it, and a failed
	/// attempt falls through to remaining candidates.
	#[must_use]
	pub fn hint(self, hint: OwnedServerName) -> Self { Self { hint: Some(hint), ..self } }

	/// Supplies a candidate pool in place of room-derived discovery.
	///
	/// The supplied servers remain subject to eligibility filtering,
	/// deduplication, and peer-reachability ranking.
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

	/// Sets the room version for Event and AuthEvent deep validation.
	///
	/// `None` keeps the V11 default, so callers from another room version must
	/// set it to avoid spurious rejection. Other operations retain the value in
	/// coalescing identity but do not consult it during validation.
	#[must_use]
	pub fn room_version(self, room_version: RoomVersionId) -> Self {
		Self { room_version: Some(room_version), ..self }
	}

	/// Caps the number of candidate servers contacted.
	///
	/// `None` permits candidate exhaustion, and each round stays within the
	/// remaining budget.
	#[must_use]
	pub fn attempt_limit(self, attempt_limit: NonZeroUsize) -> Self {
		Self {
			attempt_limit: Some(attempt_limit),
			..self
		}
	}

	/// Sets the event limit for Backfill and MissingEvents batches.
	///
	/// Other operations retain the value in coalescing identity but do not send
	/// it on the wire.
	#[must_use]
	pub fn backfill_limit(self, backfill_limit: NonZeroUsize) -> Self {
		Self {
			backfill_limit: Some(backfill_limit),
			..self
		}
	}

	/// Sets the per-round fanout width schedule.
	///
	/// The worker clamps each computed width to the candidate pool, optional
	/// ceiling, and remaining attempt budget.
	#[must_use]
	pub fn fanout(self, growth: FanoutGrowth) -> Self { Self { fanout_growth: growth, ..self } }

	/// Caps concurrent candidate attempts in each fanout round.
	///
	/// `None` lets the configured growth curve run until another budget binds.
	#[must_use]
	pub fn fanout_max_width(self, max_width: NonZeroUsize) -> Self {
		Self {
			fanout_max_width: Some(max_width),
			..self
		}
	}

	/// Caps the number of fanout escalation rounds.
	///
	/// `None` leaves the per-call cap unset; the global round cap, candidate pool,
	/// or attempt budget can still stop escalation.
	#[must_use]
	pub fn fanout_rounds(self, rounds: NonZeroUsize) -> Self {
		Self { fanout_rounds: Some(rounds), ..self }
	}

	/// Applies the operation's recommended staged fanout profile.
	///
	/// [`Opts::new`] is sequential unless the caller opts in here. AuthEvent,
	/// AuthChain, StateIds, and MissingEvents receive profiles; Event, Backfill,
	/// and TimestampToEvent remain unchanged.
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

	/// Toggles the four implemented validation gates together.
	///
	/// Passing `false` accepts transport bytes without conformance or deep PDU
	/// checks. The unused `authoritative_redaction` option remains unchanged.
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

/// Contains a raw response body and the server that supplied it.
///
/// The bytes are reference-counted so concurrent callers coalesced onto one
/// fetch share a single buffer.
#[derive(Debug)]
pub struct Outcome {
	/// Raw response body accepted by every enabled validation gate.
	pub bytes: Bytes,

	/// Server whose response won the attempt race.
	pub origin: OwnedServerName,
}
