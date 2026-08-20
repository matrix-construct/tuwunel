//! The fetch worker loop: one task owning every in-flight fetch, lock-free.
//!
//! [`Service::run_worker`] coalesces incoming requests, dispatches attempts up
//! to the capacity bound, defers the rest, and broadcasts each outcome to its
//! subscribers.

use std::{
	collections::{HashMap, VecDeque},
	sync::{Arc, Weak},
};

use bytes::Bytes;
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use ruma::OwnedServerName;
use tokio::sync::watch::channel;
use tuwunel_core::{debug_warn, implement, trace, utils::math::effective_cap};

use super::{
	Failure, Msg, Opts, Outcome, Service,
	error::Attempted,
	inflight::{Inflight, Key, SharedResult},
};

/// One in-flight fetch, borrowing the worker for the service's lifetime and
/// yielding its key alongside the result so the worker can route it.
type FetchFuture<'a> = BoxFuture<'a, (Key, SharedResult)>;
type FetchFutures<'a> = FuturesUnordered<FetchFuture<'a>>;

/// Service worker. Owns the request map, the deferral queue, and every
/// in-flight fetch on its stack, so no lock guards any of them.
#[implement(Service)]
pub(super) async fn run_worker(self: Arc<Self>) {
	let mut inflight: HashMap<Key, Inflight> = HashMap::new();
	let mut pending: VecDeque<Msg> = VecDeque::new();
	let mut futures: FetchFutures<'_> = FuturesUnordered::new();

	self.work_loop(&mut inflight, &mut pending, &mut futures)
		.await;
}

#[implement(Service)]
async fn work_loop<'a>(
	&'a self,
	inflight: &mut HashMap<Key, Inflight>,
	pending: &mut VecDeque<Msg>,
	futures: &mut FetchFutures<'a>,
) {
	let rx = self.channel.1.clone();
	while !rx.is_closed() {
		// Coalesce co-arriving callers before any completion evicts their entry.
		while let Ok(msg) = rx.try_recv() {
			self.on_request(msg, inflight, pending, futures);
		}

		tokio::select! {
			Some((key, result)) = futures.next() =>
				self.on_complete(key, result, inflight, pending, futures),
			msg = rx.recv_async() => match msg {
				| Ok(msg) => self.on_request(msg, inflight, pending, futures),
				| Err(_) => break,
			},
		}
	}
}

#[implement(Service)]
fn on_request<'a>(
	&'a self,
	msg: Msg,
	inflight: &mut HashMap<Key, Inflight>,
	pending: &mut VecDeque<Msg>,
	futures: &FetchFutures<'a>,
) {
	let Some(entry) = inflight.get_mut(&msg.key) else {
		// no in-flight request for this key: dispatch, or defer at the cap
		if futures.len() >= self.capacity {
			pending.push_back(msg);
		} else {
			self.dispatch(msg, inflight, futures);
		}

		return;
	};

	match entry.interest.upgrade() {
		// live callers: coalesce onto the running future
		| Some(strong) => {
			msg.reply
				.send((entry.tx.subscribe(), strong))
				.ok();
		},
		// every prior caller dropped and the future is draining toward
		// Cancelled; re-arm so it revives at its next attempt boundary
		| None => {
			let interest = Arc::new(());
			entry.interest = Arc::downgrade(&interest);
			msg.reply
				.send((entry.tx.subscribe(), interest))
				.ok();
		},
	}
}

#[implement(Service)]
fn dispatch<'a>(
	&'a self,
	msg: Msg,
	inflight: &mut HashMap<Key, Inflight>,
	futures: &FetchFutures<'a>,
) {
	let Msg { key, opts, reply } = msg;
	let interest = Arc::new(());
	let (tx, rx) = channel(None);

	// caller already gone: do not touch the network
	if reply.send((rx, interest.clone())).is_err() {
		return;
	}

	let opts = Arc::new(opts);
	let weak = Arc::downgrade(&interest);
	inflight.insert(key.clone(), Inflight {
		tx,
		interest: weak.clone(),
		opts: opts.clone(),
	});

	self.push_attempt(futures, key, opts, weak);
}

/// Push one attempt future onto the worker's set, yielding its key with the
/// result so the worker can route it back. The lone construction site for the
/// borrowed-future shape, shared by the fresh-dispatch and re-arm paths.
#[implement(Service)]
fn push_attempt<'a>(
	&'a self,
	futures: &FetchFutures<'a>,
	key: Key,
	opts: Arc<Opts>,
	weak: Weak<()>,
) {
	futures.push(async move { (key, self.run_attempts(&opts, &weak).await) }.boxed());
}

#[implement(Service)]
fn on_complete<'a>(
	&'a self,
	key: Key,
	result: SharedResult,
	inflight: &mut HashMap<Key, Inflight>,
	pending: &mut VecDeque<Msg>,
	futures: &FetchFutures<'a>,
) {
	let Some(entry) = inflight.get(&key) else {
		return;
	};

	// a fresh caller re-armed after the future gave up: revive from the
	// retained opts rather than publishing a stale Cancelled
	if matches!(&result, Err(Failure::Cancelled)) && entry.interest.upgrade().is_some() {
		let opts = entry.opts.clone();
		let weak = entry.interest.clone();
		self.push_attempt(futures, key, opts, weak);
		return;
	}

	entry.tx.send(Some(result)).ok();
	inflight.remove(&key);

	// A freed slot re-admits deferred requests through on_request so a deferred
	// same-key pair coalesces instead of double-dispatching.
	while futures.len() < self.capacity {
		let Some(msg) = pending.pop_front() else {
			break;
		};

		self.on_request(msg, inflight, pending, futures);
	}
}

#[implement(Service)]
#[tracing::instrument(
	name = "attempts",
	level = "debug",
	skip_all,
	fields(
		op = ?opts.op,
		room_id = ?opts.room_id,
		event_id = ?opts.event_id,
	),
)]
async fn run_attempts(&self, opts: &Opts, interest: &Weak<()>) -> SharedResult {
	let candidates = self.select.candidates(opts).await;
	if candidates.is_empty() {
		return Err(Failure::NoCandidates);
	}

	let count = candidates.len();
	let limit = opts
		.attempt_limit
		.map_or(count, |n| n.get().min(count));

	let (config_width, config_rounds) = self
		.services
		.try_get()
		.map_or((0, 0), |services| {
			let config = &services.server.config;

			(config.fetch_fanout_max_width, config.fetch_fanout_rounds)
		});

	let max_width = effective_cap(opts.fanout_max_width, config_width);
	let max_rounds = effective_cap(opts.fanout_rounds, config_rounds);

	let mut attempted: Attempted = Attempted::new();
	let mut remaining = candidates.into_iter();
	let mut round: usize = 0;

	while attempted.len() < limit {
		if interest.strong_count() == 0 {
			return Err(Failure::Cancelled);
		}

		if round >= max_rounds {
			break;
		}

		let budget = limit.saturating_sub(attempted.len());
		let width = opts
			.fanout_growth
			.round_width(round)
			.min(max_width)
			.min(budget);

		// race this round's window; the first valid response wins and dropping the
		// set cancels the losing requests in flight
		let mut racing: FuturesUnordered<_> = remaining
			.by_ref()
			.take(width)
			.map(|server| self.attempt(server, opts))
			.collect();

		if racing.is_empty() {
			break;
		}

		while let Some((server, bytes)) = racing.next().await {
			let Some(bytes) = bytes else {
				attempted.push(server);

				if interest.strong_count() == 0 {
					return Err(Failure::Cancelled);
				}

				continue;
			};

			trace!(%server, "fetch satisfied");
			return Ok(Arc::new(Outcome { bytes, origin: server }));
		}

		round = round.saturating_add(1);
	}

	Err(Failure::NotFound { attempted })
}

/// Fetch one candidate and validate it: `Some(bytes)` on a clean response,
/// `None` on a transport error or a poisoned body. A miss is logged, never
/// fatal, so it cannot cancel a sibling racing the same round.
#[implement(Service)]
#[tracing::instrument(
	name = "attempt",
	level = "trace",
	skip_all,
	fields(%server),
)]
async fn attempt(
	&self,
	server: OwnedServerName,
	opts: &Opts,
) -> (OwnedServerName, Option<Bytes>) {
	let Some(bytes) = self
		.transport
		.fetch_raw(opts.op, &server, opts)
		.await
		.inspect_err(|error| debug_warn!(%server, "fetch attempt failed: {error}"))
		.ok()
	else {
		return (server, None);
	};

	let valid = self
		.validate(opts, &bytes)
		.await
		.inspect_err(|error| debug_warn!(%server, "rejecting poisoned response: {error}"))
		.is_ok();

	(server, valid.then_some(bytes))
}
