//! Hand a connected secondary the last of the WAL before a clean stop.
//!
//! A primary's WAL streams (`GET /_tuwunel/replication/wal`) used to run until
//! the HTTP listener's graceful-shutdown timeout cut their connections, and
//! the services that write (the outbound sender, presence, request handlers)
//! were only stopped after that. Whatever they wrote while stopping never
//! reached the secondary, so the node it promoted held less than this
//! database — and fast rejoin, correctly, refused (see
//! `tuwunel_database::rejoin`).
//!
//! The shutdown now stops every writer first, then asks each open stream to
//! send everything up to the final sequence, a heartbeat at that sequence,
//! and end. [`Drain`] carries that request to the streams and lets shutdown
//! wait, for a bounded time, until they have all ended.

use std::time::Duration;

use tokio::sync::watch;

/// Coordinates open WAL streams with the shutdown that drains them.
pub struct Drain {
	/// `Some(seq)` once shutdown has stopped every writer: each open stream
	/// sends through `seq`, then ends.
	target: watch::Sender<Option<u64>>,
	/// Streams whose response body has not been dropped yet.
	open: watch::Sender<usize>,
}

/// Held by one WAL stream's response body. Dropped when the body has been
/// handed over in full or the connection went away.
pub struct StreamGuard {
	open: watch::Sender<usize>,
}

/// What [`Drain::run`] saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
	/// The sequence the streams were asked to deliver through.
	pub target: u64,
	/// Streams open when the drain started.
	pub streams: usize,
	/// Streams still open when it gave up; 0 means every stream ended.
	pub remaining: usize,
}

impl Default for Drain {
	fn default() -> Self { Self::new() }
}

impl Drain {
	#[must_use]
	pub fn new() -> Self {
		Self {
			target: watch::Sender::new(None),
			open: watch::Sender::new(0),
		}
	}

	/// Register a stream. The guard goes into the response body; the
	/// receiver tells the stream's task when, and through which sequence, to
	/// finish. A stream opened after the drain started sees the target at
	/// once.
	#[must_use]
	pub fn open(&self) -> (StreamGuard, watch::Receiver<Option<u64>>) {
		self.open.send_modify(|n| *n = n.saturating_add(1));
		let guard = StreamGuard { open: self.open.clone() };

		(guard, self.target.subscribe())
	}

	/// Streams currently open.
	#[must_use]
	pub fn open_streams(&self) -> usize { *self.open.borrow() }

	/// The sequence a drain asked for, once one has started.
	#[must_use]
	pub fn target(&self) -> Option<u64> { *self.target.borrow() }

	/// Ask every open stream to deliver through `target` and end, then wait
	/// until none is open or `timeout` passes. Never waits longer than
	/// `timeout`; an absent or stuck secondary only costs that.
	pub async fn run(&self, target: u64, timeout: Duration) -> Outcome {
		let streams = self.open_streams();
		self.target.send_replace(Some(target));

		let mut open = self.open.subscribe();
		_ = tokio::time::timeout(timeout, open.wait_for(|&n| n == 0)).await;

		Outcome {
			target,
			streams,
			remaining: self.open_streams(),
		}
	}
}

impl Drop for StreamGuard {
	fn drop(&mut self) { self.open.send_modify(|n| *n = n.saturating_sub(1)); }
}

/// Whether a stream that has sent through `sent` is finished, given the
/// drain target it last saw.
#[inline]
#[must_use]
pub fn reached(target: Option<u64>, sent: u64) -> bool { target.is_some_and(|t| sent >= t) }

#[cfg(test)]
mod tests {
	use std::time::{Duration, Instant};

	use super::*;

	#[tokio::test]
	async fn no_streams_returns_at_once() {
		let drain = Drain::new();
		let start = Instant::now();
		let out = drain.run(42, Duration::from_secs(5)).await;
		assert_eq!(out, Outcome { target: 42, streams: 0, remaining: 0 });
		assert!(start.elapsed() < Duration::from_secs(1));
		assert_eq!(drain.target(), Some(42));
	}

	/// A stream that sees the target, finishes and drops its guard ends the
	/// wait well before the timeout.
	#[tokio::test]
	async fn waits_for_streams_to_finish() {
		let drain = std::sync::Arc::new(Drain::new());
		let (guard, mut target) = drain.open();
		assert_eq!(drain.open_streams(), 1);

		let stream = tokio::spawn(async move {
			let seen = *target.wait_for(Option::is_some).await.unwrap();
			drop(guard);
			seen
		});

		let start = Instant::now();
		let out = drain.run(7, Duration::from_secs(5)).await;
		assert_eq!(out, Outcome { target: 7, streams: 1, remaining: 0 });
		assert!(start.elapsed() < Duration::from_secs(1));
		assert_eq!(stream.await.unwrap(), Some(7));
	}

	/// A stream that never ends (secondary stuck, connection hung) costs the
	/// timeout and no more, and is reported.
	#[tokio::test]
	async fn gives_up_after_timeout() {
		let drain = Drain::new();
		let (_guard, _target) = drain.open();

		let start = Instant::now();
		let out = drain.run(9, Duration::from_millis(100)).await;
		assert_eq!(out, Outcome { target: 9, streams: 1, remaining: 1 });
		let waited = start.elapsed();
		assert!(waited >= Duration::from_millis(100), "{waited:?}");
		assert!(waited < Duration::from_secs(2), "{waited:?}");
	}

	#[test]
	fn late_stream_sees_target_immediately() {
		let drain = Drain::new();
		drain.target.send_replace(Some(11));
		let (_guard, target) = drain.open();
		assert_eq!(*target.borrow(), Some(11));
	}

	#[test]
	fn guards_count_down_on_drop() {
		let drain = Drain::new();
		let (a, _) = drain.open();
		let (b, _) = drain.open();
		assert_eq!(drain.open_streams(), 2);
		drop(a);
		assert_eq!(drain.open_streams(), 1);
		drop(b);
		assert_eq!(drain.open_streams(), 0);
	}

	#[test]
	fn reached_only_once_a_target_is_set_and_met() {
		assert!(!reached(None, u64::MAX));
		assert!(!reached(Some(10), 9));
		assert!(reached(Some(10), 10));
		assert!(reached(Some(10), 11));
	}
}
