//! Single-flight bookkeeping for one in-flight fetch.
//!
//! [`Key`] is the dedup key the worker's in-flight map is keyed on;
//! [`Inflight`] is the worker-owned entry every coalesced caller subscribes to;
//! [`SharedResult`] is the broadcast outcome and [`Subscription`] the caller's
//! handle, whose liveness token cancels the fetch on drop.

use std::{
	collections::hash_map::DefaultHasher,
	hash::{Hash, Hasher},
	num::NonZeroUsize,
	sync::{Arc, Weak},
};

use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedServerName, RoomVersionId,
	api::Direction,
};
use tokio::sync::watch::{Receiver, Sender};
use tuwunel_core::implement;

use super::{Failure, FanoutGrowth, Op, Opts, Outcome};

/// Single-flight dedup key for a request and its complete caller policy.
///
/// Missing-event windows are sorted into collision-safe, order-independent
/// identities. Every other caller option compares exactly, so a joiner never
/// inherits a different request, candidate, retry, fan-out, or validation
/// policy from the flight owner.
#[derive(Clone, Debug)]
pub(super) struct Key {
	fingerprint: u64,
	opts: Arc<Opts>,
}

#[derive(Eq, Hash, PartialEq)]
struct Identity<'a> {
	op: Op,
	room_id: &'a Option<OwnedRoomId>,
	event_id: &'a Option<OwnedEventId>,
	earliest_events: &'a [OwnedEventId],
	latest_events: &'a [OwnedEventId],
	ts: &'a Option<MilliSecondsSinceUnixEpoch>,
	dir: Option<bool>,
	hint: &'a Option<OwnedServerName>,
	candidates: &'a [OwnedServerName],
	room_version: &'a Option<RoomVersionId>,
	attempt_limit: &'a Option<NonZeroUsize>,
	backfill_limit: &'a Option<NonZeroUsize>,
	fanout_growth: &'a FanoutGrowth,
	fanout_max_width: &'a Option<NonZeroUsize>,
	fanout_rounds: &'a Option<NonZeroUsize>,
	check_event_id: bool,
	check_conforms: bool,
	check_hashes: bool,
	authoritative_redaction: bool,
	check_signature: bool,
}

/// Represents the outcome shared by callers coalesced onto one fetch.
///
/// Successful responses remain cheap to broadcast because each result holds a
/// reference-counted [`Outcome`].
pub(super) type SharedResult = Result<Arc<Outcome>, Failure>;

/// Carries a caller's result channel and liveness token.
///
/// Dropping the final strong token tells the worker that it may cancel the
/// in-flight fetch.
pub(super) type Subscription = (Receiver<Option<SharedResult>>, Arc<()>);

/// Holds one worker-owned fetch and its subscribers.
///
/// The worker is the sole mutator, so no lock guards this state; coalesced
/// callers reach it only through their channels.
pub(super) struct Inflight {
	/// Result channel. Coalesced callers subscribe to await the outcome.
	pub(super) tx: Sender<Option<SharedResult>>,

	/// Liveness signal. The strong token rides to the callers; the worker holds
	/// this weak ref and the fetch bails once it can no longer upgrade it.
	pub(super) interest: Weak<()>,

	/// Retained (shared) so a re-armed key re-dispatches without re-cloning it.
	pub(super) opts: Arc<Opts>,
}

impl PartialEq for Key {
	fn eq(&self, other: &Self) -> bool {
		self.fingerprint == other.fingerprint
			&& (Arc::ptr_eq(&self.opts, &other.opts)
				|| identity(&self.opts) == identity(&other.opts))
	}
}

impl Eq for Key {}

impl Hash for Key {
	fn hash<H: Hasher>(&self, state: &mut H) { self.fingerprint.hash(state); }
}

/// Derives the single-flight key from a request's [`Opts`].
///
/// Missing-event windows are sorted before hashing so equivalent windows
/// coalesce regardless of caller ordering.
#[implement(Key)]
pub(super) fn new(mut opts: Opts) -> Self {
	if matches!(opts.op, Op::MissingEvents) {
		opts.earliest_events.sort_unstable();
		opts.latest_events.sort_unstable();
	}

	let opts = Arc::new(opts);
	let fingerprint = fingerprint(&identity(&opts));

	Self { fingerprint, opts }
}

/// Share the canonical request with the fetch worker.
///
/// Only the `Arc` is cloned; the option fields and event windows remain shared.
#[implement(Key)]
#[inline]
pub(super) fn opts(&self) -> Arc<Opts> { self.opts.clone() }

fn identity(opts: &Opts) -> Identity<'_> {
	let windows = matches!(opts.op, Op::MissingEvents)
		.then_some((opts.earliest_events.as_slice(), opts.latest_events.as_slice()))
		.unwrap_or_default();

	let (earliest_events, latest_events) = windows;
	let dir = opts
		.dir
		.map(|dir| matches!(dir, Direction::Forward));

	Identity {
		op: opts.op,
		room_id: &opts.room_id,
		event_id: &opts.event_id,
		earliest_events,
		latest_events,
		ts: &opts.ts,
		dir,
		hint: &opts.hint,
		candidates: &opts.candidates,
		room_version: &opts.room_version,
		attempt_limit: &opts.attempt_limit,
		backfill_limit: &opts.backfill_limit,
		fanout_growth: &opts.fanout_growth,
		fanout_max_width: &opts.fanout_max_width,
		fanout_rounds: &opts.fanout_rounds,
		check_event_id: opts.check_event_id,
		check_conforms: opts.check_conforms,
		check_hashes: opts.check_hashes,
		authoritative_redaction: opts.authoritative_redaction,
		check_signature: opts.check_signature,
	}
}

fn fingerprint(identity: &Identity<'_>) -> u64 {
	let mut state = DefaultHasher::new();

	identity.hash(&mut state);
	state.finish()
}
