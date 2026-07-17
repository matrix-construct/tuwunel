use std::{collections::HashSet, iter::once, num::NonZeroUsize};

use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join, try_join, try_join4},
};
use rand::seq::SliceRandom;
use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, ServerName,
	api::Direction, events::TimelineEventType,
};
use serde::Deserialize;
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{
	Result, at, debug, debug_warn, implement, is_false,
	matrix::{
		event::Event,
		pdu::{PduCount, PduId, RawPduId},
	},
	utils::{
		BoolExt, IterStream, ReadyExt,
		future::{BoolExt as FutureBoolExt, TryExtExt},
	},
	validated, warn,
};
use tuwunel_database::Json;

use super::{ExtractBody, bias_count};
use crate::{
	federation::Candidates,
	fetcher::{Op, Opts},
	rooms::state_accessor::plain_text_topic,
};

/// Events requested per backfill batch.
const BACKFILL_LIMIT: NonZeroUsize = NonZeroUsize::new(100).unwrap();

/// The `event_id` and timestamp parsed back out of an [`Op::TimestampToEvent`]
/// fetch outcome.
#[derive(Deserialize)]
struct TimestampHit {
	event_id: OwnedEventId,
	origin_server_ts: MilliSecondsSinceUnixEpoch,
}

#[implement(super::Service)]
#[tracing::instrument(name = "backfill", level = "debug", skip(self))]
pub async fn backfill_if_required(&self, room_id: &RoomId, from: PduCount) -> Result {
	let (first_pdu_count, first_pdu) = self
		.first_item_in_room(room_id)
		.await
		.expect("Room is not empty");

	// No backfill required, there are still events between them
	if first_pdu_count < from {
		return Ok(());
	}

	// No backfill required, reached the end.
	if *first_pdu.event_type() == TimelineEventType::RoomCreate {
		return Ok(());
	}

	let empty_room = self
		.services
		.state_cache
		.room_joined_count(room_id)
		.map_ok_or(true, |count| count <= 1);

	let not_world_readable = self
		.services
		.state_accessor
		.is_world_readable(room_id)
		.map(is_false!());

	// Room is empty (1 user or none), there is no one that can backfill
	if empty_room.and(not_world_readable).await {
		return Ok(());
	}

	let eligible = self.backfill_candidates(room_id).await;

	let no_backfill = || {
		warn!(%room_id, "No servers could backfill, but backfill was needed");
		Ok(())
	};

	// Empty here, rather than deferring to the fetcher, keeps backfill scoped to
	// the authoritative servers; the fetcher would otherwise fall back to the
	// room's whole population.
	if eligible.is_empty() {
		return no_backfill();
	}

	let opts = Opts::new(Op::Backfill, room_id.to_owned())
		.event_id(first_pdu.event_id().to_owned())
		.candidates(eligible)
		.backfill_limit(BACKFILL_LIMIT);

	let Ok(outcome) = self
		.services
		.fetcher
		.fetch(opts)
		.inspect_err(|e| warn!(%room_id, "Backfilling failed: {e}"))
		.await
	else {
		return no_backfill();
	};

	let pdus: Vec<Box<RawJsonValue>> = serde_json::from_slice(&outcome.bytes)?;

	pdus.into_iter()
		.stream()
		.for_each(async |pdu| {
			self.backfill_pdu(room_id, &outcome.origin, pdu)
				.await
				.inspect_err(|e| debug_warn!(%room_id, "Failed to add backfilled pdu: {e}"))
				.ok();
		})
		.await;

	Ok(())
}

#[implement(super::Service)]
async fn backfill_candidates(&self, room_id: &RoomId) -> Candidates {
	let canonical_alias = self
		.services
		.state_accessor
		.get_canonical_alias(room_id);

	let power_levels = self
		.services
		.state_accessor
		.get_power_levels(room_id);

	let (canonical_alias, power_levels) = join(canonical_alias, power_levels).await;

	let power_servers = power_levels
		.iter()
		.flat_map(|power| {
			power
				.rules
				.privileged_creators
				.iter()
				.flat_map(|creators| creators.iter())
		})
		.chain(power_levels.iter().flat_map(|power| {
			power
				.users
				.iter()
				.filter_map(|(user_id, level)| level.gt(&power.users_default).then_some(user_id))
		}))
		.filter_map(|user_id| {
			self.services
				.globals
				.user_is_local(user_id)
				.is_false()
				.then_some(user_id.server_name())
		})
		.collect::<HashSet<_>>();

	let power_servers = {
		let mut vec: Vec<_> = power_servers
			.into_iter()
			.map(ToOwned::to_owned)
			.collect();

		vec.shuffle(&mut rand::rng());
		vec.into_iter().stream()
	};

	let canonical_room_alias_server = once(canonical_alias)
		.filter_map(Result::ok)
		.map(|alias| alias.server_name().to_owned())
		.stream();

	let trusted_servers = self
		.services
		.server
		.config
		.trusted_servers
		.iter()
		.map(ToOwned::to_owned)
		.stream();

	power_servers
		.chain(canonical_room_alias_server)
		.chain(trusted_servers)
		.ready_filter(|server_name| !self.services.globals.server_is_ours(server_name))
		.filter_map(async |server_name| {
			self.services
				.state_cache
				.server_in_room(&server_name, room_id)
				.await
				.then_some(server_name)
		})
		.collect()
		.await
}

#[implement(super::Service)]
pub async fn get_event_id_near_ts_with_fallback(
	&self,
	room_id: &RoomId,
	ts: MilliSecondsSinceUnixEpoch,
	dir: Direction,
) -> Result<(MilliSecondsSinceUnixEpoch, OwnedEventId)> {
	let local = self.get_event_id_near_ts(room_id, ts, dir).await;

	// Federate on a local miss, or a forward hit at the start edge of our history.
	let federate = match &local {
		| Err(_) => true,
		| Ok((_, event_id)) =>
			dir == Direction::Forward && self.is_start_edge_hit(room_id, event_id).await,
	};

	if !federate {
		return local;
	}

	let candidates = self.backfill_candidates(room_id).await;
	if candidates.is_empty() {
		return local;
	}

	let opts = Opts::new(Op::TimestampToEvent, room_id.to_owned())
		.ts(ts)
		.dir(dir)
		.candidates(candidates)
		.checks(false);

	let Ok(outcome) = self.services.fetcher.fetch(opts).await else {
		return local;
	};

	let Ok(TimestampHit { event_id, origin_server_ts }) = serde_json::from_slice(&outcome.bytes)
	else {
		return local;
	};

	// Keep the local hit when it is no farther from the timestamp than the remote.
	if let Ok((local_ts, local_id)) = &local
		&& !nearer(dir, origin_server_ts, *local_ts)
	{
		return Ok((*local_ts, local_id.clone()));
	}

	// Fail closed: an un-ingested event can't be visibility-checked, so keep local.
	let Ok(()) = self
		.backfill_event(room_id, &event_id, &outcome.origin)
		.await
		.inspect_err(|e| debug_warn!(%room_id, "timestamp fallback backfill failed: {e}"))
	else {
		return local;
	};

	Ok((origin_server_ts, event_id))
}

#[implement(super::Service)]
async fn is_start_edge_hit(&self, room_id: &RoomId, event_id: &EventId) -> bool {
	self.first_item_in_room(room_id)
		.await
		.is_ok_and(|(_, first)| {
			*first.event_type() != TimelineEventType::RoomCreate && first.event_id() == event_id
		})
}

/// Whether `a` is nearer the queried timestamp than `b` for a search in `dir`.
fn nearer(dir: Direction, a: MilliSecondsSinceUnixEpoch, b: MilliSecondsSinceUnixEpoch) -> bool {
	match dir {
		| Direction::Forward => a < b,
		| Direction::Backward => a > b,
	}
}

#[implement(super::Service)]
async fn backfill_event(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	origin: &ServerName,
) -> Result {
	let opts = Opts::new(Op::Backfill, room_id.to_owned())
		.event_id(event_id.to_owned())
		.candidates([origin.to_owned()])
		.backfill_limit(BACKFILL_LIMIT);

	let outcome = self.services.fetcher.fetch(opts).await?;

	let pdus: Vec<Box<RawJsonValue>> = serde_json::from_slice(&outcome.bytes)?;

	pdus.into_iter()
		.stream()
		.for_each(async |pdu| {
			self.backfill_pdu(room_id, &outcome.origin, pdu)
				.await
				.inspect_err(|e| debug_warn!(%room_id, "Failed to add backfilled pdu: {e}"))
				.ok();
		})
		.await;

	Ok(())
}

/// Fetch a single event we have not received over federation and persist it via
/// the backfill path, so a subsequent local lookup resolves it. Checks are off:
/// `backfill_pdu` performs full signature, hash, and auth validation itself.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn fetch_remote_event(&self, room_id: &RoomId, event_id: &EventId) -> Result {
	let opts = Opts::new(Op::Event, room_id.to_owned())
		.event_id(event_id.to_owned())
		.checks(false);

	let outcome = self.services.fetcher.fetch(opts).await?;

	let pdu: Box<RawJsonValue> = serde_json::from_slice(&outcome.bytes)?;

	self.backfill_pdu(room_id, &outcome.origin, pdu)
		.await
}

#[implement(super::Service)]
#[tracing::instrument(skip(self, pdu), level = "debug")]
pub async fn backfill_pdu(
	&self,
	room_id: &RoomId,
	origin: &ServerName,
	pdu: Box<RawJsonValue>,
) -> Result {
	let parsed = self
		.services
		.event_handler
		.parse_incoming_pdu(&pdu);

	// Lock so we cannot backfill the same pdu twice at the same time
	let mutex_lock = self
		.services
		.event_handler
		.mutex_federation
		.lock(room_id)
		.map(Ok);

	let ((_, event_id, value), mutex_lock) = try_join(parsed, mutex_lock).await?;

	let existed = self
		.services
		.event_handler
		.handle_incoming_pdu(origin, room_id, &event_id, value, false)
		.await?
		.map(at!(1))
		.is_some_and(is_false!());

	// Bail if the PDU already exists; a duplicate insertion is not good.
	if existed {
		return Ok(());
	}

	let pdu = self.get_pdu(&event_id);

	let value = self.get_pdu_json(&event_id);

	let shortroomid = self.services.short.get_shortroomid(room_id);

	let insert_lock = self.mutex_insert.lock(room_id).map(Ok);

	let (pdu, value, shortroomid, insert_lock) =
		try_join4(pdu, value, shortroomid, insert_lock).await?;

	// A pdu_id is not returned from handle_incoming_pdu() when accepting a new
	// event on this codepath. The pdu_id is instead created here in ℤ−
	let count = self.services.globals.next_count();
	let count: i64 = (*count).try_into()?;
	let pdu_id: RawPduId = PduId {
		shortroomid,
		count: PduCount::Backfilled(validated!(0 - count)),
	}
	.into();

	// Insert pdu
	self.prepend_backfill_pdu(
		&pdu_id,
		room_id,
		&event_id,
		u64::from(pdu.origin_server_ts),
		&value,
	);
	drop(insert_lock);

	match pdu.kind {
		| TimelineEventType::RoomMessage => {
			let content: ExtractBody = pdu.get_content()?;
			if let Some(body) = content.body {
				self.services
					.search
					.index_pdu(shortroomid, &pdu_id, &body);
			}
		},
		| TimelineEventType::RoomTopic =>
			if let Some(topic) = pdu.get_content().ok().and_then(plain_text_topic) {
				self.services
					.search
					.index_pdu(shortroomid, &pdu_id, &topic);
			},
		| _ => {},
	}

	drop(mutex_lock);

	debug!("Prepended backfill pdu");
	Ok(())
}

#[implement(super::Service)]
fn prepend_backfill_pdu(
	&self,
	pdu_id: &RawPduId,
	room_id: &RoomId,
	event_id: &EventId,
	origin_server_ts: u64,
	json: &CanonicalJsonObject,
) {
	self.db.pduid_pdu.raw_put(pdu_id, Json(json));

	self.db.eventid_pduid.insert(event_id, pdu_id);

	self.db.eventid_outlierpdu.remove(event_id);

	let count_key = bias_count(pdu_id.count());

	self.db
		.roomid_tscount_pducount
		.put_raw((room_id, origin_server_ts, count_key), pdu_id.count());
}
