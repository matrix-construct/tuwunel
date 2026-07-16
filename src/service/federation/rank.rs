//! Verdict-ranked candidate selection shared by the fetcher and backfill.
//!
//! A caller hands an eligibility-filtered, priority-ordered server list to
//! [`rank_candidates`]; the per-server [`ShouldAttempt`] verdict reorders and
//! drops it.

use futures::StreamExt;
use ruma::OwnedServerName;
use tuwunel_core::{debug_warn, implement, smallvec::SmallVec, utils::IterStream};

use super::ShouldAttempt;

/// A candidate server pool for one fetch: the hint, room servers, and mxid
/// hosts. Inline budget 3 holds the common single-event case (event origin,
/// room origin, optional hint) on the stack; larger room-derived pools spill
/// to the heap.
pub type Candidates = SmallVec<[OwnedServerName; 3]>;

/// A candidate pool paired with each server's verdict, sorted and filtered in
/// place by [`rank_from_verdicts`].
type Verdicts = SmallVec<[(OwnedServerName, ShouldAttempt); 3]>;

/// Behavior when every candidate is backed off (no `Yes` or `Deprioritize`
/// verdict in the pool).
#[derive(Clone, Copy, Debug)]
pub enum WhenAllBackedOff {
	/// Attempt the backed-off servers anyway, so a transient backoff never
	/// becomes a permanent local unresolved-event state.
	Attempt,

	/// Drop them; the pool collapses to empty.
	Fail,
}

/// Gather each candidate's [`ShouldAttempt`] verdict and rank `eligible`,
/// preserving the caller's order within each verdict bucket.
#[implement(super::Service)]
pub async fn rank_candidates(
	&self,
	eligible: Candidates,
	when_all: WhenAllBackedOff,
) -> Candidates {
	let verdicts: Verdicts = eligible
		.into_iter()
		.stream()
		.then(async |server| {
			let verdict = self.should_attempt(&server).await;
			(server, verdict)
		})
		.collect()
		.await;

	rank_from_verdicts(verdicts, when_all).collect()
}

/// Order `Yes` before `Deprioritize` before `No`, preserving input order
/// within each rank, then drop `No` unless every candidate is backed off and
/// `when_all` is `Attempt`.
fn rank_from_verdicts(
	mut verdicts: Verdicts,
	when_all: WhenAllBackedOff,
) -> impl Iterator<Item = OwnedServerName> {
	let all_backed_off = verdicts
		.iter()
		.all(|(_, verdict)| matches!(verdict, ShouldAttempt::No { .. }));

	let keep_backed_off = all_backed_off && matches!(when_all, WhenAllBackedOff::Attempt);

	if keep_backed_off && !verdicts.is_empty() {
		debug_warn!(
			n = verdicts.len(),
			"All candidates backed off via peer_status; attempting anyway"
		);
	}

	verdicts.sort_by_key(|(_, verdict)| verdict.rank());

	verdicts
		.into_iter()
		.filter(move |(_, verdict)| {
			keep_backed_off || !matches!(verdict, ShouldAttempt::No { .. })
		})
		.map(|(server, _)| server)
}

/// Sort ordinal placing `Yes` ahead of `Deprioritize` ahead of `No`.
#[implement(ShouldAttempt)]
#[inline]
fn rank(self) -> u8 {
	match self {
		| ShouldAttempt::Yes => 0,
		| ShouldAttempt::Deprioritize => 1,
		| ShouldAttempt::No { .. } => 2,
	}
}

#[cfg(test)]
mod tests {
	use std::time::SystemTime;

	use ruma::{OwnedServerName, owned_server_name};
	use tuwunel_core::smallvec::smallvec;

	use super::{Verdicts, WhenAllBackedOff, rank_from_verdicts};
	use crate::federation::ShouldAttempt;

	fn no() -> ShouldAttempt { ShouldAttempt::No { earliest_retry: SystemTime::UNIX_EPOCH } }

	fn names(servers: &[OwnedServerName]) -> Vec<&str> {
		servers.iter().map(AsRef::as_ref).collect()
	}

	#[test]
	fn all_yes_preserves_order() {
		let verdicts: Verdicts = smallvec![
			(owned_server_name!("a.test"), ShouldAttempt::Yes),
			(owned_server_name!("b.test"), ShouldAttempt::Yes),
			(owned_server_name!("c.test"), ShouldAttempt::Yes),
		];

		let ranked: Vec<_> = rank_from_verdicts(verdicts, WhenAllBackedOff::Attempt).collect();

		assert_eq!(names(&ranked), ["a.test", "b.test", "c.test"]);
	}

	#[test]
	fn drops_backed_off_when_pool_has_alternatives() {
		let verdicts: Verdicts = smallvec![
			(owned_server_name!("a.test"), ShouldAttempt::Yes),
			(owned_server_name!("b.test"), no()),
			(owned_server_name!("c.test"), ShouldAttempt::Yes),
		];

		let ranked: Vec<_> = rank_from_verdicts(verdicts, WhenAllBackedOff::Attempt).collect();

		assert_eq!(names(&ranked), ["a.test", "c.test"]);
	}

	#[test]
	fn all_backed_off_attempt_falls_through() {
		let verdicts: Verdicts = smallvec![
			(owned_server_name!("a.test"), no()),
			(owned_server_name!("b.test"), no()),
		];

		let ranked: Vec<_> = rank_from_verdicts(verdicts, WhenAllBackedOff::Attempt).collect();

		assert_eq!(names(&ranked), ["a.test", "b.test"]);
	}

	#[test]
	fn all_backed_off_fail_returns_empty() {
		let verdicts: Verdicts = smallvec![
			(owned_server_name!("a.test"), no()),
			(owned_server_name!("b.test"), no()),
		];

		assert!(
			rank_from_verdicts(verdicts, WhenAllBackedOff::Fail)
				.next()
				.is_none()
		);
	}

	#[test]
	fn deprioritize_ranks_after_yes() {
		let verdicts: Verdicts = smallvec![
			(owned_server_name!("d.test"), ShouldAttempt::Deprioritize),
			(owned_server_name!("y.test"), ShouldAttempt::Yes),
			(owned_server_name!("n.test"), no()),
		];

		let ranked: Vec<_> = rank_from_verdicts(verdicts, WhenAllBackedOff::Attempt).collect();

		assert_eq!(names(&ranked), ["y.test", "d.test"]);
	}
}
