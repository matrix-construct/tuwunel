use std::{collections::HashSet, iter::once};

use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join, try_join, try_join4},
};
use rand::seq::SliceRandom;
use ruma::{
	CanonicalJsonObject, EventId, RoomId, ServerName, api::federation, events::TimelineEventType,
	uint,
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{
	Result, at, debug, debug_info, debug_warn, implement, is_false,
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

use super::ExtractBody;

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

		vec.shuffle(&mut rand::thread_rng());
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

	let mut servers = power_servers
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
		.boxed();

	while let Some(ref backfill_server) = servers.next().await {
		let request = federation::backfill::get_backfill::v1::Request {
			room_id: room_id.to_owned(),
			v: vec![first_pdu.event_id().to_owned()],
			limit: uint!(100),
		};

		debug_info!("Asking {backfill_server} for backfill");
		if let Ok(response) = self
			.services
			.federation
			.execute(backfill_server, request)
			.inspect_err(|e| {
				warn!("{backfill_server} failed backfilling for room {room_id}: {e}");
			})
			.await
		{
			return response
				.pdus
				.into_iter()
				.stream()
				.for_each(async |pdu| {
					if let Err(e) = self
						.backfill_pdu(room_id, backfill_server, pdu)
						.await
					{
						debug_warn!("Failed to add backfilled pdu in room {room_id}: {e}");
					}
				})
				.map(Ok)
				.await;
		}
	}

	warn!("No servers could backfill, but backfill was needed in room {room_id}");

	Ok(())
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
		.boxed()
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

	if pdu.kind == TimelineEventType::RoomMessage {
		let content: ExtractBody = pdu.get_content()?;
		if let Some(body) = content.body {
			self.services
				.search
				.index_pdu(shortroomid, &pdu_id, &body);
		}
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

	self.db
		.roomid_ts_pducount
		.put_raw((room_id, origin_server_ts), pdu_id.count());
}
