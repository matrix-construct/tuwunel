//! Retrieves older room history from federation and inserts validated events.
//!
//! Candidate servers are selected from trusted and room-related origins, while
//! remote timestamp claims are checked against the ingested event. Backfilled
//! events receive negative stream counts so they sort before normal history.

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
	Err, Result, at, debug, debug_warn, implement, is_false,
	matrix::{
		PduEvent,
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

const BACKFILL_ATTEMPT_LIMIT: NonZeroUsize = NonZeroUsize::new(5).unwrap();

const BACKFILL_BATCH_ATTEMPTS: usize = 3;

/// The `event_id` and timestamp parsed back out of an [`Op::TimestampToEvent`]
/// fetch outcome.
#[derive(Deserialize)]
struct TimestampHit {
	event_id: OwnedEventId,
	origin_server_ts: MilliSecondsSinceUnixEpoch,
}

/// Attempts federation backfill when a request reaches local history's edge.
///
/// Backfill is skipped after the create event and for effectively empty rooms
/// that are not world-readable. No candidate, a remote fetch failure, or an
/// empty accepted chunk is treated as a successful best-effort outcome.
#[implement(super::Service)]
#[tracing::instrument(name = "backfill", level = "debug", skip(self))]
pub async fn backfill_if_required(&self, room_id: &RoomId, from: PduCount) -> Result {
	let (first_pdu_count, first_pdu) = self.first_item_in_room(room_id).await?;

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

	let mut eligible = self.backfill_candidates(room_id).await;

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

	for _ in 0..BACKFILL_BATCH_ATTEMPTS {
		let opts = Opts::new(Op::Backfill, room_id.to_owned())
			.event_id(first_pdu.event_id().to_owned())
			.candidates(eligible.iter().cloned())
			.attempt_limit(BACKFILL_ATTEMPT_LIMIT)
			.backfill_limit(BACKFILL_LIMIT);

		let Ok(outcome) = self.services.fetcher.fetch(opts).await else {
			return no_backfill();
		};

		let pdus: Vec<Box<RawJsonValue>> = serde_json::from_slice(&outcome.bytes)?;
		let batch_size = pdus.len();
		let prepended = pdus
			.into_iter()
			.stream()
			.fold(0_usize, async |prepended, pdu| {
				let inserted = self
					.backfill_pdu(room_id, &outcome.origin, pdu)
					.await
					.inspect_err(|e| debug_warn!(%room_id, %e, "Failed to add backfilled pdu"))
					.unwrap_or(false);

				prepended.saturating_add(usize::from(inserted))
			})
			.await;

		debug!(
			%room_id,
			origin = %outcome.origin,
			batch_size,
			prepended,
			"Processed backfill response",
		);

		if prepended > 0 {
			return Ok(());
		}

		eligible.retain(|server| server != &outcome.origin);
		if eligible.is_empty() {
			break;
		}
	}

	warn!(%room_id, "Backfill was required but prepended no events");
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
/// Finds the nearest event to a timestamp with a federation fallback.
///
/// The local answer is retained unless a closer remote claim can be ingested.
/// The ingested event must belong to this room and its actual timestamp must
/// remain on the requested side of the query.
pub async fn get_event_id_near_ts_with_fallback(
	&self,
	room_id: &RoomId,
	ts: MilliSecondsSinceUnixEpoch,
	dir: Direction,
) -> Result<(MilliSecondsSinceUnixEpoch, OwnedEventId)> {
	let local = self.get_event_id_near_ts(room_id, ts, dir).await;

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

	if let Ok((local_ts, local_id)) = &local
		&& !nearer(dir, origin_server_ts, *local_ts)
	{
		return Ok((*local_ts, local_id.clone()));
	}

	// Fail closed: an un-ingested event can't be visibility-checked, so keep local.
	let Ok(pdu) = self
		.backfill_event(room_id, &event_id, &outcome.origin)
		.inspect_err(|e| debug_warn!(%room_id, error = ?e, "timestamp fallback backfill failed"))
		.await
	else {
		return local;
	};

	let actual_ts = pdu.origin_server_ts();
	let matches_direction = match dir {
		| Direction::Forward => actual_ts >= ts,
		| Direction::Backward => actual_ts <= ts,
	};

	if actual_ts != origin_server_ts || !matches_direction {
		debug_warn!(
			%room_id,
			%event_id,
			?dir,
			?ts,
			?origin_server_ts,
			?actual_ts,
			"timestamp fallback claim was inconsistent with the ingested event"
		);
		return local;
	}

	Ok((actual_ts, event_id))
}

#[implement(super::Service)]
async fn is_start_edge_hit(&self, room_id: &RoomId, event_id: &EventId) -> bool {
	self.first_item_in_room(room_id)
		.await
		.is_ok_and(|(_, first)| {
			*first.event_type() != TimelineEventType::RoomCreate && first.event_id() == event_id
		})
}

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
) -> Result<PduEvent> {
	let opts = Opts::new(Op::Backfill, room_id.to_owned())
		.event_id(event_id.to_owned())
		.candidates([origin.to_owned()])
		.backfill_limit(BACKFILL_LIMIT);

	let outcome = self.services.fetcher.fetch(opts).await?;

	let pdus: Vec<Box<RawJsonValue>> = serde_json::from_slice(&outcome.bytes)?;

	let ingestion = pdus
		.into_iter()
		.stream()
		.fold(Ok(()), async |prior, pdu| {
			let current = self
				.backfill_pdu(room_id, &outcome.origin, pdu)
				.map_ok(|_| ())
				.inspect_err(
					|e| debug_warn!(%room_id, error = ?e, "Failed to add backfilled pdu"),
				)
				.await;

			prior.and(current)
		})
		.await;

	match self.get_pdu_count(event_id).await {
		| Err(error) => ingestion.and(Err(error)),
		| Ok(_) => {
			let pdu = self.get_pdu(event_id).await?;

			if pdu.room_id() != room_id {
				return Err!(Request(NotFound(
					"Timestamp fallback target belongs to another room."
				)));
			}

			Ok(pdu)
		},
	}
}

/// Fetches one remote event and persists it through the backfill path.
///
/// Fetcher checks are disabled only for retrieval; `backfill_pdu`
/// performs signature, hash, and authorization validation. An event already
/// accepted completes successfully without another insertion, while a stored
/// outlier is still validated and promoted.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn fetch_remote_event(&self, room_id: &RoomId, event_id: &EventId) -> Result {
	let opts = Opts::new(Op::Event, room_id.to_owned())
		.event_id(event_id.to_owned())
		.checks(false);

	let outcome = self.services.fetcher.fetch(opts).await?;

	let pdu: Box<RawJsonValue> = serde_json::from_slice(&outcome.bytes)?;

	self.backfill_pdu(room_id, &outcome.origin, pdu)
		.await?;

	Ok(())
}

/// Validates and inserts one remotely supplied event as backfilled history.
///
/// An already accepted duplicate returns `false` without insertion. A new event
/// or stored outlier receives a negative backfill count, moves to accepted
/// storage, updates its timestamp and ID indexes, and adds searchable message
/// or topic content.
#[implement(super::Service)]
#[tracing::instrument(skip(self, pdu), level = "debug")]
pub async fn backfill_pdu(
	&self,
	room_id: &RoomId,
	origin: &ServerName,
	pdu: Box<RawJsonValue>,
) -> Result<bool> {
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
		return Ok(false);
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
			if let Ok(ExtractBody { body: Some(body) }) = pdu.get_content() {
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
	Ok(true)
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
	let mut txn = self.db.db.txn();

	txn.raw_put(&self.db.pduid_pdu, pdu_id, Json(json));
	txn.insert_raw(&self.db.eventid_pduid, event_id, pdu_id);
	txn.del_raw(&self.db.eventid_outlierpdu, event_id);

	let count_key = bias_count(pdu_id.count());
	let key = (room_id, origin_server_ts, count_key);
	txn.put_raw(&self.db.roomid_tscount_pducount, key, pdu_id.count());

	txn.execute();
}
