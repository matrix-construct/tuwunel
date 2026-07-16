use std::{collections::HashMap, num::NonZeroUsize, sync::Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use ruma::{
	OwnedEventId, OwnedRoomId, OwnedServerName, RoomVersionId, ServerName, event_id, room_id,
	server_name,
};
use serde_json::value::RawValue as RawJsonValue;
use tokio::{
	sync::Notify,
	task::{spawn, yield_now},
};
use tuwunel_core::err;

use super::{worker::effective_cap, *};
use crate::federation::Candidates;

impl Service {
	fn test(
		transport: Arc<dyn Transport>,
		select: Arc<dyn Select>,
		capacity: usize,
	) -> Arc<Self> {
		Arc::new(Self {
			services: Arc::new(OnceServices::default()),
			channel: unbounded(),
			transport,
			select,
			capacity,
		})
	}

	fn test_spawn(
		transport: Arc<dyn Transport>,
		select: Arc<dyn Select>,
		capacity: usize,
	) -> Arc<Self> {
		let svc = Self::test(transport, select, capacity);
		spawn(svc.clone().run_worker());
		svc
	}
}

/// A two-element JSON array, the batch shape `Op::Backfill` /
/// `Op::MissingEvents` return; the bytes a [`Behavior::Batch`] server answers
/// with.
const BATCH_BODY: &[u8] = br#"[{"x":1},{"y":2}]"#;

enum Behavior {
	/// Valid JSON object.
	Good,

	/// Invalid JSON; fails the conform check.
	Garbage,

	/// Transport-level failure.
	Fail,

	/// Valid JSON array of two events ([`BATCH_BODY`]).
	Batch,

	/// Block until released, then answer with garbage.
	BlockGarbage,

	/// Block until released, then answer with a valid JSON object.
	BlockGood,
}

struct MockTransport {
	behaviors: HashMap<OwnedServerName, Behavior>,
	calls: Mutex<Vec<OwnedServerName>>,
	seen_opts: Mutex<Vec<Opts>>,
	dropped: Mutex<Vec<OwnedServerName>>,
	releases: HashMap<OwnedServerName, Notify>,
}

/// Records a server whose in-flight fetch was cancelled (future dropped before
/// it was released), so a fan-out test can prove the loser of a race was
/// aborted.
struct CancelGuard<'a> {
	dropped: &'a Mutex<Vec<OwnedServerName>>,
	server: OwnedServerName,
	armed: bool,
}

impl Drop for CancelGuard<'_> {
	fn drop(&mut self) {
		if self.armed {
			self.dropped
				.lock()
				.expect("dropped not poisoned")
				.push(self.server.clone());
		}
	}
}

impl MockTransport {
	fn new(behaviors: impl IntoIterator<Item = (OwnedServerName, Behavior)>) -> Self {
		let behaviors: HashMap<OwnedServerName, Behavior> = behaviors.into_iter().collect();
		let releases = behaviors
			.iter()
			.filter(|(_, behavior)| {
				matches!(behavior, Behavior::BlockGarbage | Behavior::BlockGood)
			})
			.map(|(server, _)| (server.clone(), Notify::new()))
			.collect();

		Self {
			behaviors,
			calls: Mutex::new(Vec::new()),
			seen_opts: Mutex::new(Vec::new()),
			dropped: Mutex::new(Vec::new()),
			releases,
		}
	}

	fn calls(&self) -> Vec<OwnedServerName> {
		self.calls
			.lock()
			.expect("calls not poisoned")
			.clone()
	}

	fn last_opts(&self) -> Opts {
		self.seen_opts
			.lock()
			.expect("seen_opts not poisoned")
			.last()
			.expect("at least one fetch recorded")
			.clone()
	}

	fn dropped(&self) -> Vec<OwnedServerName> {
		self.dropped
			.lock()
			.expect("dropped not poisoned")
			.clone()
	}

	fn call_count(&self) -> usize {
		self.calls
			.lock()
			.expect("calls not poisoned")
			.len()
	}

	fn calls_to(&self, server: &ServerName) -> usize {
		self.calls
			.lock()
			.expect("calls not poisoned")
			.iter()
			.filter(|called| called.as_str() == server.as_str())
			.count()
	}

	fn release(&self, server: &ServerName) {
		self.releases
			.get(server)
			.expect("server has a pending block to release")
			.notify_one();
	}
}

#[async_trait]
impl Transport for MockTransport {
	async fn fetch_raw(&self, _op: Op, server: &ServerName, opts: &Opts) -> Result<Bytes> {
		self.calls
			.lock()
			.expect("calls not poisoned")
			.push(server.to_owned());

		self.seen_opts
			.lock()
			.expect("seen_opts not poisoned")
			.push(opts.clone());

		match self
			.behaviors
			.get(server)
			.unwrap_or(&Behavior::Fail)
		{
			| Behavior::Good => Ok(Bytes::from_static(b"{\"ok\":true}")),
			| Behavior::Garbage => Ok(Bytes::from_static(b"not json")),
			| Behavior::Fail => Err(err!("mock transport failure")),
			| Behavior::Batch => Ok(Bytes::from_static(BATCH_BODY)),
			| Behavior::BlockGarbage => {
				let mut guard = CancelGuard {
					dropped: &self.dropped,
					server: server.to_owned(),
					armed: true,
				};

				self.blocked(server).await;
				guard.armed = false;
				Ok(Bytes::from_static(b"not json"))
			},
			| Behavior::BlockGood => {
				let mut guard = CancelGuard {
					dropped: &self.dropped,
					server: server.to_owned(),
					armed: true,
				};

				self.blocked(server).await;
				guard.armed = false;
				Ok(Bytes::from_static(b"{\"ok\":true}"))
			},
		}
	}
}

impl MockTransport {
	async fn blocked(&self, server: &ServerName) {
		self.releases
			.get(server)
			.expect("blocking server has a release")
			.notified()
			.await;
	}
}

struct MockSelect {
	by_event: HashMap<OwnedEventId, Vec<OwnedServerName>>,
	fixed: Vec<OwnedServerName>,
}

impl MockSelect {
	fn new(by_event: impl IntoIterator<Item = (OwnedEventId, Vec<OwnedServerName>)>) -> Self {
		Self {
			by_event: by_event.into_iter().collect(),
			fixed: Vec::new(),
		}
	}

	/// Return one fixed pool for every fetch, regardless of `event_id`; the
	/// room-scoped ops carry no event to key on.
	fn fixed(servers: impl IntoIterator<Item = OwnedServerName>) -> Self {
		Self {
			by_event: HashMap::new(),
			fixed: servers.into_iter().collect(),
		}
	}
}

#[async_trait]
impl Select for MockSelect {
	async fn candidates(&self, opts: &Opts) -> Candidates {
		if !self.fixed.is_empty() {
			return self.fixed.iter().cloned().collect();
		}

		opts.event_id
			.as_ref()
			.and_then(|event| self.by_event.get(event))
			.map(|servers| servers.iter().cloned().collect())
			.unwrap_or_default()
	}
}

fn room() -> OwnedRoomId { room_id!("!room:test.local").to_owned() }

fn nz(n: usize) -> NonZeroUsize { NonZeroUsize::new(n).expect("nonzero") }

/// Eight distinct candidate servers, returned in a stable order so a fan-out
/// test can predict which servers each round pulls.
fn pool8() -> Vec<OwnedServerName> {
	Vec::from(
		[
			server_name!("s0.test.local"),
			server_name!("s1.test.local"),
			server_name!("s2.test.local"),
			server_name!("s3.test.local"),
			server_name!("s4.test.local"),
			server_name!("s5.test.local"),
			server_name!("s6.test.local"),
			server_name!("s7.test.local"),
		]
		.map(ToOwned::to_owned),
	)
}

fn test_opts(event: &OwnedEventId) -> Opts {
	Opts {
		check_event_id: false,
		check_hashes: false,
		check_signature: false,
		..Opts::new(Op::Event, room()).event_id(event.clone())
	}
}

async fn settle() {
	for _ in 0..16 {
		yield_now().await;
	}
}

async fn wait_for(mut cond: impl FnMut() -> bool) {
	for _ in 0..1000 {
		if cond() {
			return;
		}

		yield_now().await;
	}

	panic!("condition was never met");
}

#[tokio::test]
async fn coalesces_concurrent_fetches() {
	let server = server_name!("a.test.local").to_owned();
	let event = event_id!("$ev:test.local").to_owned();

	let mock = Arc::new(MockTransport::new([(server.clone(), Behavior::Good)]));
	let select = Arc::new(MockSelect::new([(event.clone(), vec![server.clone()])]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let opts = test_opts(&event);
	let (a, b) = tokio::join!(svc.fetch(opts.clone()), svc.fetch(opts));

	let a = a.expect("first fetch ok");
	let b = b.expect("second fetch ok");

	assert_eq!(a.origin, server);
	assert_eq!(b.origin, server);
	assert!(Arc::ptr_eq(&a, &b), "coalesced callers share one outcome");
	assert_eq!(mock.call_count(), 1, "one network attempt for two callers");
}

#[tokio::test]
async fn fails_over_past_poisoned_server() {
	let a = server_name!("a.test.local").to_owned();
	let b = server_name!("b.test.local").to_owned();
	let event = event_id!("$ev:test.local").to_owned();

	let mock = Arc::new(MockTransport::new([
		(a.clone(), Behavior::Garbage),
		(b.clone(), Behavior::Good),
	]));
	let select = Arc::new(MockSelect::new([(event.clone(), vec![a.clone(), b.clone()])]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let outcome = svc
		.fetch(test_opts(&event))
		.await
		.expect("fails over to the good server");

	assert_eq!(outcome.origin, b);
	assert_eq!(mock.calls(), vec![a, b], "poisoned server attempted before the good one");
}

#[tokio::test]
async fn errors_when_all_servers_fail() {
	let a = server_name!("a.test.local").to_owned();
	let b = server_name!("b.test.local").to_owned();
	let event = event_id!("$ev:test.local").to_owned();

	let mock =
		Arc::new(MockTransport::new([(a.clone(), Behavior::Fail), (b.clone(), Behavior::Fail)]));
	let select = Arc::new(MockSelect::new([(event.clone(), vec![a.clone(), b.clone()])]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let error = svc
		.fetch(test_opts(&event))
		.await
		.expect_err("no server has the event");

	assert!(error.to_string().contains("not found"), "unexpected error: {error}");
	assert_eq!(mock.calls(), vec![a, b], "both servers attempted before giving up");
}

#[tokio::test]
async fn capacity_backpressures_extra_keys() {
	let a = server_name!("a.test.local").to_owned();
	let b = server_name!("b.test.local").to_owned();
	let ev1 = event_id!("$one:test.local").to_owned();
	let ev2 = event_id!("$two:test.local").to_owned();

	let mock = Arc::new(MockTransport::new([
		(a.clone(), Behavior::BlockGarbage),
		(b.clone(), Behavior::Good),
	]));
	let select = Arc::new(MockSelect::new([
		(ev1.clone(), vec![a.clone()]),
		(ev2.clone(), vec![b.clone()]),
	]));
	let svc = Service::test_spawn(mock.clone(), select, 1);

	let g1 = {
		let svc = svc.clone();
		let opts = test_opts(&ev1);
		spawn(async move { svc.fetch(opts).await })
	};

	wait_for(|| mock.call_count() >= 1).await;

	let g2 = {
		let svc = svc.clone();
		let opts = test_opts(&ev2);
		spawn(async move { svc.fetch(opts).await })
	};

	settle().await;
	assert_eq!(mock.call_count(), 1, "second key cannot start while the slot is held");
	assert!(!g2.is_finished());

	mock.release(&a);

	drop(g1.await.expect("join g1"));
	let r2 = g2
		.await
		.expect("join g2")
		.expect("second key fetched once the slot freed");

	assert_eq!(r2.origin, b);
	assert_eq!(mock.call_count(), 2);
}

#[tokio::test]
async fn worker_stops_when_callers_drop() {
	let a = server_name!("a.test.local").to_owned();
	let b = server_name!("b.test.local").to_owned();
	let event = event_id!("$ev:test.local").to_owned();

	let mock = Arc::new(MockTransport::new([
		(a.clone(), Behavior::BlockGarbage),
		(b.clone(), Behavior::Good),
	]));
	let select = Arc::new(MockSelect::new([(event.clone(), vec![a.clone(), b.clone()])]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let g = {
		let svc = svc.clone();
		let opts = test_opts(&event);
		spawn(async move { svc.fetch(opts).await })
	};

	wait_for(|| mock.call_count() >= 1).await;

	// drop the only caller; at its next attempt boundary the worker sees the
	// dead interest token and abandons the fetch before trying B
	g.abort();
	drop(g.await);

	mock.release(&a);
	settle().await;

	assert_eq!(mock.calls(), vec![a], "worker aborts at the attempt boundary; B untried");
}

#[tokio::test]
async fn coalesces_same_key_deferred_under_backpressure() {
	// Two callers for one key, both deferred while the cap is saturated by
	// other keys, must still collapse onto a single network attempt once slots
	// free, and neither may observe a spurious cancellation.
	let b1 = server_name!("b1.test.local").to_owned();
	let b2 = server_name!("b2.test.local").to_owned();
	let t = server_name!("t.test.local").to_owned();
	let evb1 = event_id!("$b1:test.local").to_owned();
	let evb2 = event_id!("$b2:test.local").to_owned();
	let target = event_id!("$target:test.local").to_owned();

	let mock = Arc::new(MockTransport::new([
		(b1.clone(), Behavior::BlockGarbage),
		(b2.clone(), Behavior::BlockGarbage),
		(t.clone(), Behavior::BlockGood),
	]));
	let select = Arc::new(MockSelect::new([
		(evb1.clone(), vec![b1.clone()]),
		(evb2.clone(), vec![b2.clone()]),
		(target.clone(), vec![t.clone()]),
	]));
	let svc = Service::test_spawn(mock.clone(), select, 2);

	// saturate both slots with distinct keys
	let gb1 = {
		let svc = svc.clone();
		let opts = test_opts(&evb1);
		spawn(async move { svc.fetch(opts).await })
	};
	let gb2 = {
		let svc = svc.clone();
		let opts = test_opts(&evb2);
		spawn(async move { svc.fetch(opts).await })
	};

	wait_for(|| mock.call_count() >= 2).await;

	// both same-key callers arrive while at capacity, so both are deferred with
	// no in-flight entry to coalesce onto yet
	let gt1 = {
		let svc = svc.clone();
		let opts = test_opts(&target);
		spawn(async move { svc.fetch(opts).await })
	};
	let gt2 = {
		let svc = svc.clone();
		let opts = test_opts(&target);
		spawn(async move { svc.fetch(opts).await })
	};

	settle().await;
	assert_eq!(mock.call_count(), 2, "deferred callers do not start while saturated");

	// free one slot: the first deferred caller dispatches and blocks on t
	mock.release(&b1);
	wait_for(|| mock.calls_to(&t) >= 1).await;
	assert_eq!(mock.calls_to(&t), 1, "first deferred caller dispatched once");

	// free the second slot: the second deferred caller must coalesce onto the
	// still-in-flight target fetch, not dispatch a second one
	mock.release(&b2);
	settle().await;
	assert_eq!(mock.calls_to(&t), 1, "second deferred caller coalesced; no second attempt");

	// release the target: both callers receive the one shared outcome
	mock.release(&t);
	let r1 = gt1
		.await
		.expect("join gt1")
		.expect("first target caller ok");
	let r2 = gt2
		.await
		.expect("join gt2")
		.expect("second target caller ok");

	assert_eq!(r1.origin, t);
	assert!(Arc::ptr_eq(&r1, &r2), "deferred callers share one outcome");
	assert_eq!(mock.calls_to(&t), 1, "exactly one network attempt for both callers");

	drop(gb1.await);
	drop(gb2.await);
}

#[tokio::test]
async fn room_version_threads_into_event_id_check() {
	// A v1 event carries an explicit event_id that gen_event_id passes through,
	// where the V11 reference-hash algorithm derives a different id. The threaded
	// room version must accept the valid event; the prior V11 pin rejected it.
	let svc = Service::test(Arc::new(MockTransport::new([])), Arc::new(MockSelect::new([])), 4);

	let event = event_id!("$explicit:test.local").to_owned();
	let bytes = br#"{
		"auth_events": [],
		"content": { "body": "hello" },
		"depth": 1,
		"event_id": "$explicit:test.local",
		"origin": "test.local",
		"origin_server_ts": 1000,
		"prev_events": [],
		"room_id": "!room:test.local",
		"sender": "@alice:test.local",
		"type": "m.room.message"
	}"#;

	let versioned = Opts {
		check_hashes: false,
		check_signature: false,
		..Opts::new(Op::Event, room())
			.event_id(event.clone())
			.room_version(RoomVersionId::V1)
	};

	svc.validate(&versioned, bytes)
		.await
		.expect("v1 event accepted when its room version is named");

	let pinned = Opts { room_version: None, ..versioned.clone() };

	let error = svc
		.validate(&pinned, bytes)
		.await
		.expect_err("rejected under the V11 default");

	assert!(error.to_string().contains("wrong event id"), "unexpected error: {error}");
}

#[test]
fn round_width_schedules() {
	let fixed = FanoutGrowth::Fixed(nz(1));
	assert_eq!(fixed.round_width(0), 1);
	assert_eq!(fixed.round_width(7), 1);

	let linear = FanoutGrowth::Linear { base: nz(1), step: nz(1) };
	assert_eq!(linear.round_width(0), 1);
	assert_eq!(linear.round_width(1), 2);
	assert_eq!(linear.round_width(2), 3);

	let geometric = FanoutGrowth::Geometric { base: nz(1), factor: nz(2) };
	assert_eq!(geometric.round_width(0), 1);
	assert_eq!(geometric.round_width(1), 2);
	assert_eq!(geometric.round_width(2), 4);
	assert_eq!(geometric.round_width(3), 8);

	// a runaway exponent saturates rather than overflowing
	assert_eq!(geometric.round_width(1000), usize::MAX);
}

#[test]
fn fanout_for_op_profiles() {
	let profile = |op| Opts::new(op, room()).fanout_for_op();

	let auth_event = profile(Op::AuthEvent);
	assert_eq!(auth_event.fanout_growth, FanoutGrowth::Geometric { base: nz(1), factor: nz(2) });
	assert_eq!(auth_event.fanout_max_width, Some(nz(4)));
	assert_eq!(auth_event.fanout_rounds, Some(nz(5)));

	let auth_chain = profile(Op::AuthChain);
	assert_eq!(auth_chain.fanout_growth, FanoutGrowth::Linear { base: nz(1), step: nz(1) });
	assert_eq!(auth_chain.fanout_max_width, Some(nz(2)));
	assert_eq!(auth_chain.fanout_rounds, Some(nz(2)));

	let state_ids = profile(Op::StateIds);
	assert_eq!(state_ids.fanout_growth, FanoutGrowth::Linear { base: nz(1), step: nz(1) });
	assert_eq!(state_ids.fanout_max_width, Some(nz(3)));
	assert_eq!(state_ids.fanout_rounds, Some(nz(3)));

	let missing = profile(Op::MissingEvents);
	assert_eq!(missing.fanout_growth, FanoutGrowth::Geometric { base: nz(1), factor: nz(2) });
	assert_eq!(missing.fanout_max_width, None, "prev_events ramp is unbounded");
	assert_eq!(missing.fanout_rounds, Some(nz(3)));

	for op in [Op::Event, Op::Backfill] {
		let dark = profile(op);
		assert_eq!(dark.fanout_growth, FanoutGrowth::Fixed(nz(1)), "{op:?} stays sequential");
		assert_eq!(dark.fanout_max_width, None);
		assert_eq!(dark.fanout_rounds, None);
	}
}

#[test]
fn effective_cap_clamps() {
	assert_eq!(effective_cap(Some(nz(4)), 0), 4);
	assert_eq!(effective_cap(None, 0), usize::MAX);

	assert_eq!(effective_cap(Some(nz(4)), 2), 2, "config tightens the opts cap");
	assert_eq!(effective_cap(Some(nz(2)), 4), 2, "config never widens the opts cap");
	assert_eq!(effective_cap(None, 3), 3, "config bounds an unbounded profile");
}

#[tokio::test]
async fn default_opts_attempts_sequentially() {
	// The Opts::new default (Fixed(1)) must hold exactly one request in flight: B
	// is not raced while A is still pending.
	let a = server_name!("a.test.local").to_owned();
	let b = server_name!("b.test.local").to_owned();
	let event = event_id!("$ev:test.local").to_owned();

	let mock = Arc::new(MockTransport::new([
		(a.clone(), Behavior::BlockGarbage),
		(b.clone(), Behavior::Good),
	]));
	let select = Arc::new(MockSelect::new([(event.clone(), vec![a.clone(), b.clone()])]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let g = {
		let svc = svc.clone();
		let opts = test_opts(&event);
		spawn(async move { svc.fetch(opts).await })
	};

	wait_for(|| mock.call_count() >= 1).await;
	settle().await;
	assert_eq!(mock.calls(), vec![a.clone()], "B not raced while A is in flight");

	mock.release(&a);
	let outcome = g
		.await
		.expect("join")
		.expect("fails over to the good server");

	assert_eq!(outcome.origin, b);
	assert_eq!(mock.calls(), vec![a, b], "sequential fallover, candidate order");
}

#[tokio::test]
async fn max_width_caps_round_concurrency() {
	// Geometric growth wants 1, 2, 4, ... but a ceiling of 2 holds every round at
	// most 2 wide, so after three rounds exactly 1 + 2 + 2 servers were contacted.
	let pool = pool8();
	let event = event_id!("$ev:test.local").to_owned();

	let behaviors = pool
		.iter()
		.cloned()
		.map(|server| (server, Behavior::BlockGarbage));
	let mock = Arc::new(MockTransport::new(behaviors));
	let select = Arc::new(MockSelect::new([(event.clone(), pool.clone())]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let opts = test_opts(&event)
		.fanout(FanoutGrowth::Geometric { base: nz(1), factor: nz(2) })
		.fanout_max_width(nz(2));
	let g = {
		let svc = svc.clone();
		spawn(async move { svc.fetch(opts).await })
	};

	wait_for(|| mock.call_count() >= 1).await;
	settle().await;
	assert_eq!(mock.call_count(), 1, "round 0 opens one");

	mock.release(&pool[0]);
	wait_for(|| mock.call_count() >= 3).await;
	settle().await;
	assert_eq!(mock.call_count(), 3, "round 1 opens two");

	mock.release(&pool[1]);
	mock.release(&pool[2]);
	wait_for(|| mock.call_count() >= 5).await;
	settle().await;
	assert_eq!(mock.call_count(), 5, "round 2 opens two (ceiling), not four (curve)");

	g.abort();
	drop(g.await);
}

#[tokio::test]
async fn unbounded_round_races_remaining_budget() {
	// With no ceiling, a fast curve's later round races every remaining candidate
	// at once: round 1 (width 4) sweeps all three servers left after round 0.
	let pool = pool8()[..4].to_vec();
	let event = event_id!("$ev:test.local").to_owned();

	let behaviors = pool
		.iter()
		.cloned()
		.map(|server| (server, Behavior::BlockGarbage));
	let mock = Arc::new(MockTransport::new(behaviors));
	let select = Arc::new(MockSelect::new([(event.clone(), pool.clone())]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let opts = test_opts(&event).fanout(FanoutGrowth::Geometric { base: nz(1), factor: nz(4) });
	let g = {
		let svc = svc.clone();
		spawn(async move { svc.fetch(opts).await })
	};

	wait_for(|| mock.call_count() >= 1).await;
	settle().await;
	assert_eq!(mock.call_count(), 1, "round 0 opens one");

	mock.release(&pool[0]);
	wait_for(|| mock.call_count() >= 4).await;
	settle().await;
	assert_eq!(mock.call_count(), 4, "unbounded round races all three remaining at once");

	g.abort();
	drop(g.await);
}

#[tokio::test]
async fn race_winner_cancels_loser() {
	// Two servers race in one round; the winner's valid response drops the round's
	// future set, which must cancel the slower in-flight request.
	let fast = server_name!("fast.test.local").to_owned();
	let slow = server_name!("slow.test.local").to_owned();
	let event = event_id!("$ev:test.local").to_owned();

	let mock = Arc::new(MockTransport::new([
		(fast.clone(), Behavior::BlockGood),
		(slow.clone(), Behavior::BlockGood),
	]));
	let select = Arc::new(MockSelect::new([(event.clone(), vec![fast.clone(), slow.clone()])]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let opts = test_opts(&event).fanout(FanoutGrowth::Fixed(nz(2)));
	let g = {
		let svc = svc.clone();
		spawn(async move { svc.fetch(opts).await })
	};

	wait_for(|| mock.call_count() >= 2).await;

	mock.release(&fast);
	let outcome = g.await.expect("join").expect("fast server wins");

	settle().await;
	assert_eq!(outcome.origin, fast);
	assert_eq!(mock.dropped(), vec![slow], "the slow loser was cancelled in flight");
}

#[tokio::test]
async fn poisoned_sibling_does_not_abort_valid() {
	// A garbage response inside a multi-server round is a miss, not a winner, and
	// must not cancel a valid sibling still in flight.
	let poison = server_name!("poison.test.local").to_owned();
	let good = server_name!("good.test.local").to_owned();
	let event = event_id!("$ev:test.local").to_owned();

	let mock = Arc::new(MockTransport::new([
		(poison.clone(), Behavior::Garbage),
		(good.clone(), Behavior::BlockGood),
	]));
	let select = Arc::new(MockSelect::new([(event.clone(), vec![poison.clone(), good.clone()])]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let opts = test_opts(&event).fanout(FanoutGrowth::Fixed(nz(2)));
	let g = {
		let svc = svc.clone();
		spawn(async move { svc.fetch(opts).await })
	};

	wait_for(|| mock.call_count() >= 2).await;

	mock.release(&good);
	let outcome = g
		.await
		.expect("join")
		.expect("valid sibling wins despite the poisoned peer");

	settle().await;
	assert_eq!(outcome.origin, good);
	assert!(mock.dropped().is_empty(), "the valid sibling was not cancelled");
}

#[tokio::test]
async fn attempt_limit_caps_total_contacts() {
	// A wide curve cannot exceed attempt_limit: Geometric over five servers with a
	// limit of 3 contacts exactly three before giving up.
	let pool = pool8()[..5].to_vec();
	let event = event_id!("$ev:test.local").to_owned();

	let behaviors = pool
		.iter()
		.cloned()
		.map(|server| (server, Behavior::Fail));
	let mock = Arc::new(MockTransport::new(behaviors));
	let select = Arc::new(MockSelect::new([(event.clone(), pool.clone())]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let opts = test_opts(&event)
		.fanout(FanoutGrowth::Geometric { base: nz(1), factor: nz(2) })
		.attempt_limit(nz(3));

	let error = svc
		.fetch(opts)
		.await
		.expect_err("no server has the event");

	assert!(error.to_string().contains("not found"), "unexpected error: {error}");
	assert_eq!(mock.call_count(), 3, "attempt_limit caps total contacts across rounds");
	assert_eq!(mock.calls(), pool[..3].to_vec(), "first three candidates, in order");
}

#[test]
fn missing_events_key_folds_order_and_separates_window() {
	let a = event_id!("$a:test.local").to_owned();
	let b = event_id!("$b:test.local").to_owned();
	let key = |opts: &Opts| Key::new(opts);

	let forward = Opts::new(Op::MissingEvents, room()).latest_events([a.clone(), b.clone()]);
	let reverse = Opts::new(Op::MissingEvents, room()).latest_events([b.clone(), a.clone()]);
	assert_eq!(key(&forward), key(&reverse), "the window key folds request order");

	let a_only = Opts::new(Op::MissingEvents, room()).latest_events([a.clone()]);
	assert_ne!(key(&forward), key(&a_only), "a different window is a different key");

	let latest_a = Opts::new(Op::MissingEvents, room())
		.latest_events([a.clone()])
		.earliest_events([b.clone()]);
	let latest_b = Opts::new(Op::MissingEvents, room())
		.latest_events([b])
		.earliest_events([a]);
	assert_ne!(key(&latest_a), key(&latest_b), "earliest and latest do not commute");
}

#[tokio::test]
async fn missing_events_request_carries_window() {
	let server = server_name!("s.test.local").to_owned();
	let latest = [event_id!("$l1:test.local").to_owned(), event_id!("$l2:test.local").to_owned()];
	let earliest = [event_id!("$e1:test.local").to_owned()];

	let mock = Arc::new(MockTransport::new([(server.clone(), Behavior::Batch)]));
	let select = Arc::new(MockSelect::fixed([server.clone()]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let opts = Opts::new(Op::MissingEvents, room())
		.latest_events(latest.clone())
		.earliest_events(earliest.clone())
		.backfill_limit(nz(25));

	svc.fetch(opts)
		.await
		.expect("windowed batch fetched");

	let seen = mock.last_opts();
	assert_eq!(seen.op, Op::MissingEvents);
	assert_eq!(
		seen.latest_events.as_slice(),
		latest.as_slice(),
		"latest window threaded intact"
	);
	assert_eq!(
		seen.earliest_events.as_slice(),
		earliest.as_slice(),
		"earliest window threaded intact"
	);
	assert_eq!(seen.backfill_limit, Some(nz(25)), "limit threaded intact");
}

#[tokio::test]
async fn missing_events_batch_round_trips() {
	let server = server_name!("s.test.local").to_owned();
	let mock = Arc::new(MockTransport::new([(server.clone(), Behavior::Batch)]));
	let select = Arc::new(MockSelect::fixed([server.clone()]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let opts = Opts::new(Op::MissingEvents, room())
		.latest_events([event_id!("$l:test.local").to_owned()]);

	let outcome = svc.fetch(opts).await.expect("batch fetched");

	assert_eq!(&*outcome.bytes, BATCH_BODY, "the batch body round-trips verbatim");

	let events: Vec<Box<RawJsonValue>> =
		serde_json::from_slice(&outcome.bytes).expect("batch parses as a pdu array");
	assert_eq!(events.len(), 2, "both events survive the round-trip");
}

#[tokio::test]
async fn backfill_parses_pdu_batch() {
	let server = server_name!("s.test.local").to_owned();
	let mock = Arc::new(MockTransport::new([(server.clone(), Behavior::Batch)]));
	let select = Arc::new(MockSelect::fixed([server.clone()]));
	let svc = Service::test_spawn(mock.clone(), select, 4);

	let opts = Opts::new(Op::Backfill, room())
		.event_id(event_id!("$first:test.local").to_owned())
		.backfill_limit(nz(100));

	let outcome = svc.fetch(opts).await.expect("backfill fetched");

	let pdus: Vec<Box<RawJsonValue>> =
		serde_json::from_slice(&outcome.bytes).expect("backfill parses as a pdu array");

	assert_eq!(pdus.len(), 2, "both pdus survive the round-trip");
	assert_eq!(outcome.origin, server, "the answering server is reported");
}
