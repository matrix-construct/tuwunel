//! Candidate server selection for a fetch.
//!
//! The [`Select`] seam enumerates the server pool; [`RoomCandidates`] derives
//! it from room state and orders it by population, pinning the room's authority
//! server ahead of the ranking for auth fetches.

use std::sync::Arc;

use async_trait::async_trait;
use futures::{Stream, StreamExt, future::Either, stream::empty};
use ruma::{EventId, OwnedServerName, RoomId, ServerName};
use tuwunel_core::{
	arrayvec::ArrayVec,
	implement,
	utils::{BoolExt, IterStream, ReadyExt, StreamTools, rand::index},
};

use super::{Op, Opts};
use crate::{
	federation::{Candidates, WhenAllBackedOff},
	services::OnceServices,
};

/// Population-ranked servers kept per fetch; bounds the serial federation
/// fan-out per missing event.
const ROUTE_FANOUT: usize = 5;

/// Candidate enumeration seam. The production impl derives the server pool from
/// room state; tests substitute a fixed list.
#[async_trait]
pub(super) trait Select: Send + Sync {
	async fn candidates(&self, opts: &Opts) -> Candidates;
}

pub(super) struct RoomCandidates {
	pub(super) services: Arc<OnceServices>,
}

#[async_trait]
impl Select for RoomCandidates {
	#[tracing::instrument(
		level = "trace",
		skip_all,
		fields(
			room_id = ?opts.room_id,
		),
	)]
	async fn candidates(&self, opts: &Opts) -> Candidates {
		if !opts.candidates.is_empty() {
			return self.ranked_override(opts).await;
		}

		let authority = self.authority_server(opts).await;

		let mxid_hosts = [
			opts.event_id
				.as_deref()
				.and_then(EventId::server_name),
			opts.room_id
				.as_deref()
				.and_then(RoomId::server_name),
		]
		.into_iter()
		.flatten()
		.map(ToOwned::to_owned);

		let popular = match opts.room_id.as_deref() {
			| None => Either::Right(empty::<OwnedServerName>()),
			| Some(room_id) => Either::Left(self.route_by_popularity(room_id).await),
		};

		let eligible = opts
			.hint
			.clone()
			.into_iter()
			.chain(authority)
			.stream()
			.chain(popular)
			.chain(mxid_hosts.stream())
			.ready_filter(|server| self.is_eligible(server));

		self.rank_unique(eligible).await
	}
}

/// Rank a caller-supplied candidate pool in place of the room-derived one,
/// filtering the ineligible (our own server, forbidden remotes). The hint, if
/// any, still leads.
#[implement(RoomCandidates)]
#[tracing::instrument(level = "trace", skip_all)]
async fn ranked_override(&self, opts: &Opts) -> Candidates {
	let eligible = opts
		.hint
		.iter()
		.chain(opts.candidates.iter())
		.filter(|&server| self.is_eligible(server))
		.cloned()
		.stream();

	self.rank_unique(eligible).await
}

/// Dedup an assembled candidate stream, preserving first-occurrence order,
/// then order it by peer-status reachability.
#[implement(RoomCandidates)]
async fn rank_unique<S>(&self, eligible: S) -> Candidates
where
	S: Stream<Item = OwnedServerName> + Send,
{
	let ordered: Candidates = eligible
		.ready_fold(Candidates::new(), push_unique)
		.await;

	self.services
		.federation
		.rank_candidates(ordered, WhenAllBackedOff::Attempt)
		.await
}

/// Append a server to the pool only on its first occurrence.
fn push_unique(mut ordered: Candidates, server: OwnedServerName) -> Candidates {
	if !ordered.contains(&server) {
		ordered.push(server);
	}

	ordered
}

/// The room's most-powerful server, pinned ahead of the population ranking
/// for auth-event and auth-chain fetches only.
#[implement(RoomCandidates)]
#[tracing::instrument(level = "trace", skip_all)]
async fn authority_server(&self, opts: &Opts) -> Option<OwnedServerName> {
	let room_id = opts.room_id.as_deref()?;

	matches!(opts.op, Op::AuthEvent | Op::AuthChain)
		.then_async(|| {
			self.services
				.state_cache
				.most_powerful_user_server(room_id)
		})
		.await
		.flatten()
}

/// Participating servers sampled in proportion to their resident member
/// count: each draw lands on a random member, so a server appears with
/// probability proportional to its population, without ranking the whole
/// membership. Falls back to the participating-server set when the room
/// has no resident members.
#[implement(RoomCandidates)]
#[tracing::instrument(level = "trace", skip_all)]
async fn route_by_popularity<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = OwnedServerName> + Send + 'a {
	let sampled: ArrayVec<OwnedServerName, ROUTE_FANOUT> = self
		.services
		.state_cache
		.room_members(room_id)
		.sample_by(|user| user.server_name().to_owned())
		.await;

	if sampled.is_empty() {
		return Either::Right(
			self.services
				.state_cache
				.room_servers(room_id)
				.map(ToOwned::to_owned),
		);
	}

	Either::Left(sampled.into_iter().stream())
}

/// Uniform-random window over the participating-server cursor: count, skip
/// a uniform offset, then take a small run. Fully lazy, with no popularity
/// aggregation. Retained (unused) as the distinctness-favoring alternative
/// to `route_by_popularity` for a future per-round re-sampling escalation.
#[implement(RoomCandidates)]
#[expect(dead_code)]
async fn route_uniformly<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = OwnedServerName> + Send + 'a {
	let count = self
		.services
		.state_cache
		.room_servers(room_id)
		.count()
		.await;

	let offset = index(count);

	self.services
		.state_cache
		.room_servers(room_id)
		.map(ToOwned::to_owned)
		.skip(offset)
		.take(ROUTE_FANOUT)
}

#[implement(RoomCandidates)]
fn is_eligible(&self, server: &ServerName) -> bool {
	!self.services.globals.server_is_ours(server)
		&& !self
			.services
			.server
			.config
			.is_forbidden_remote_server_name(server)
}

#[cfg(test)]
mod tests {
	use ruma::owned_server_name;

	use super::{Candidates, push_unique};

	#[test]
	fn push_unique_keeps_first_occurrence() {
		let pool = [
			owned_server_name!("a.test"),
			owned_server_name!("b.test"),
			owned_server_name!("a.test"),
			owned_server_name!("c.test"),
			owned_server_name!("b.test"),
		];

		let deduped: Candidates = pool
			.into_iter()
			.fold(Candidates::new(), push_unique);

		let names: Vec<&str> = deduped.iter().map(AsRef::as_ref).collect();

		assert_eq!(names, ["a.test", "b.test", "c.test"]);
	}
}
