//! Bounded federation fanout for surveys and broadcasts.
//!
//! The driver preserves one typed outcome per destination while leaving
//! destination policy, validation, and result aggregation to its callers.

mod fold;
#[cfg(test)]
mod tests;

use std::{num::NonZeroUsize, time::Duration};

use futures::{Stream, StreamExt, future::Either};
use ruma::{OwnedServerName, RoomId, ServerName, api::OutgoingRequest};
use tokio::time::{Instant, timeout};
use tuwunel_core::{
	Error, Result, implement,
	utils::{
		math::effective_cap,
		stream::{BroadbandExt, ReadyExt},
	},
};

pub use self::fold::{Faults, Grid, Origins, OutcomeExt, Tally};
use super::{
	Classification,
	scheme::{FedAuth, FedPath},
};

const WIDTH_DEFAULT: NonZeroUsize = NonZeroUsize::new(32).expect("width is nonzero");
const TIMEOUT_DEFAULT: Duration = Duration::from_secs(15);

/// Policy for one federation fanout.
///
/// Service methods resolve omitted limits from configuration, while direct
/// [`fanout_with`] calls use built-in limits. Room sweeps include the local
/// server by default. No sweep deadline is set and peer status is untouched.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
	/// Maximum number of requests in flight.
	pub width: Option<NonZeroUsize>,

	/// Deadline for each destination request.
	pub timeout: Option<Duration>,

	/// Wall-clock budget for dispatching the complete sweep.
	pub sweep_deadline: Option<Duration>,

	/// Remove the local server before constructing a room sweep.
	pub exclude_self: bool,

	/// Select whether outcomes update federation peer status.
	pub record: Record,
}

/// Peer-status behavior for one federation fanout.
///
/// Diagnostic surveys observe outcomes without changing routing state. Existing
/// traffic can opt into ordinary success and failure recording.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Record {
	/// Leave peer status untouched.
	#[default]
	Observe,

	/// Record successes and classifiable failures.
	Contribute,
}

/// One destination's settled federation request.
///
/// The origin and elapsed duration remain available for successful responses
/// and every failure class.
#[derive(Debug)]
pub struct Outcome<R> {
	/// Server contacted by this request.
	pub origin: OwnedServerName,

	/// Time from dispatch until settlement.
	pub elapsed: Duration,

	/// Typed response or the reason no response was available.
	pub result: Result<R, Fault>,
}

/// Reason a federation destination produced no response.
///
/// Deadlines remain distinct from transport and protocol errors so folds can
/// account for attempted and undispatched destinations separately.
#[derive(Debug)]
pub enum Fault {
	/// The destination request exceeded its deadline.
	Elapsed,

	/// The sweep deadline expired before dispatch.
	NotAttempted,

	/// Federation transport or protocol failure.
	Error(Error),
}

/// Surveys every server participating in a room.
///
/// The local server participates unless `exclude_self` removes it before the
/// sweep. Cursor-backed server names become owned before concurrent polling.
#[implement(super::Service)]
pub fn for_room<'a, F, R>(
	&'a self,
	room_id: &'a RoomId,
	make: F,
	opts: Opts,
) -> impl Stream<Item = Outcome<R::IncomingResponse>> + Send + 'a
where
	F: Fn(&ServerName) -> R + Send + 'a,
	R: OutgoingRequest + Send + 'a,
	R::IncomingResponse: Send,
	R::Authentication: FedAuth,
	R::PathBuilder: FedPath,
{
	let dests = self
		.services
		.state_cache
		.room_servers(room_id)
		.ready_filter(move |server| {
			!opts.exclude_self || !self.services.globals.server_is_ours(server)
		})
		.map(ToOwned::to_owned);

	self.fanout_to(dests, make, opts)
}

/// Builds one request for every destination and drives the resulting pairs.
///
/// Request construction remains sequential and lazy, while only the returned
/// federation futures are polled concurrently.
#[implement(super::Service)]
pub fn fanout_to<'a, D, F, R>(
	&'a self,
	dests: D,
	make: F,
	opts: Opts,
) -> impl Stream<Item = Outcome<R::IncomingResponse>> + Send + 'a
where
	D: Stream<Item = OwnedServerName> + Send + 'a,
	F: Fn(&ServerName) -> R + Send + 'a,
	R: OutgoingRequest + Send + 'a,
	R::IncomingResponse: Send,
	R::Authentication: FedAuth,
	R::PathBuilder: FedPath,
{
	let pairs = dests.map(move |origin| {
		let request = make(&origin);

		(origin, request)
	});

	self.fanout(pairs, opts)
}

/// Drives arbitrary destination and federation request pairs.
///
/// Every consumed pair settles as one completion-ordered outcome unless the
/// caller drops the stream and cancels remaining work. The selected recording
/// posture applies independently to each request.
#[implement(super::Service)]
#[expect(
	closure_returning_async_block,
	reason = "the capturing async closure would not implement the reusable Fn bound"
)]
pub fn fanout<'a, S, R>(
	&'a self,
	pairs: S,
	opts: Opts,
) -> impl Stream<Item = Outcome<R::IncomingResponse>> + Send + 'a
where
	S: Stream<Item = (OwnedServerName, R)> + Send + 'a,
	R: OutgoingRequest + Send + 'a,
	R::IncomingResponse: Send,
	R::Authentication: FedAuth,
	R::PathBuilder: FedPath,
{
	let config = &self.services.server.config;
	let opts = resolve_opts(opts, config.feds_max_width, config.feds_timeout);
	let client = &self.services.client.federation;
	let record = opts.record;

	fanout_with(
		pairs,
		move |dest, request| async move {
			match record {
				| Record::Observe =>
					self.execute_uncounted_allow_self(client, &dest, request)
						.await,
				| Record::Contribute =>
					self.execute_on_allow_self(client, &dest, request)
						.await,
			}
		},
		opts,
	)
	.inspect(move |outcome| {
		if record == Record::Contribute && matches!(&outcome.result, Err(Fault::Elapsed)) {
			self.record_failure(&outcome.origin, Classification::Transient);
		}
	})
}

fn resolve_opts(opts: Opts, config_width: usize, config_timeout: u64) -> Opts {
	let width = if opts.width.is_none() && config_width == 0 {
		WIDTH_DEFAULT
	} else {
		NonZeroUsize::new(effective_cap(opts.width, config_width))
			.expect("effective cap is nonzero")
	};

	Opts {
		width: Some(width),
		timeout: Some(
			opts.timeout
				.unwrap_or_else(|| Duration::from_secs(config_timeout)),
		),
		..opts
	}
}

/// Drives destination and payload pairs through a supplied asynchronous send.
///
/// Results are yielded in completion order. Every consumed pair produces one
/// outcome unless the caller drops the stream and cancels the remaining work.
pub fn fanout_with<S, I, T, F, Fut>(
	pairs: S,
	send: F,
	opts: Opts,
) -> impl Stream<Item = Outcome<T>> + Send
where
	S: Stream<Item = (OwnedServerName, I)> + Send,
	F: Fn(OwnedServerName, I) -> Fut + Send,
	Fut: Future<Output = Result<T>> + Send,
	I: Send,
	T: Send,
{
	let width = opts.width.unwrap_or(WIDTH_DEFAULT).get();
	let request_timeout = opts.timeout.unwrap_or(TIMEOUT_DEFAULT);
	let deadline = opts
		.sweep_deadline
		.and_then(|duration| Instant::now().checked_add(duration));

	pairs
		.map(move |(origin, payload)| {
			let start = Instant::now();

			if deadline.is_some_and(|deadline| start >= deadline) {
				Either::Left(Outcome {
					origin,
					elapsed: Duration::ZERO,
					result: Err(Fault::NotAttempted),
				})
			} else {
				let remaining = deadline.map_or(request_timeout, |deadline| {
					deadline
						.saturating_duration_since(start)
						.min(request_timeout)
				});
				let future = send(origin.clone(), payload);

				Either::Right((origin, start, remaining, future))
			}
		})
		.broadn_then(width, async move |request| match request {
			| Either::Left(outcome) => outcome,
			| Either::Right((origin, start, remaining, future)) => {
				let result = match timeout(remaining, future).await {
					| Ok(result) => result.map_err(Fault::Error),
					| Err(_elapsed) => Err(Fault::Elapsed),
				};

				Outcome { origin, elapsed: start.elapsed(), result }
			},
		})
}
