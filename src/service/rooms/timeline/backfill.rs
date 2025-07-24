use std::iter::once;

use futures::{FutureExt, StreamExt};
use ruma::{
	RoomId, ServerName,
	api::federation,
	events::{
		StateEventType, TimelineEventType, room::power_levels::RoomPowerLevelsEventContent,
	},
	uint,
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{
	Result, debug, debug_warn, implement, info,
	matrix::{
		event::Event,
		pdu::{PduCount, PduId, RawPduId},
	},
	utils::{IterStream, ReadyExt},
	validated, warn,
};

use super::ExtractBody;

#[implement(super::Service)]
#[tracing::instrument(name = "backfill", level = "debug", skip(self))]
pub async fn backfill_if_required(&self, room_id: &RoomId, from: PduCount) -> Result {
	if self
		.services
		.state_cache
		.room_joined_count(room_id)
		.await
		.is_ok_and(|count| count <= 1)
		&& !self
			.services
			.state_accessor
			.is_world_readable(room_id)
			.await
	{
		// Room is empty (1 user or none), there is no one that can backfill
		return Ok(());
	}

	let first_pdu = self
		.first_item_in_room(room_id)
		.await
		.expect("Room is not empty");

	if first_pdu.0 < from {
		// No backfill required, there are still events between them
		return Ok(());
	}

	let power_levels: RoomPowerLevelsEventContent = self
		.services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomPowerLevels, "")
		.await
		.unwrap_or_default();

	let room_mods = power_levels
		.users
		.iter()
		.filter_map(|(user_id, level)| {
			if level > &power_levels.users_default
				&& !self.services.globals.user_is_local(user_id)
			{
				Some(user_id.server_name())
			} else {
				None
			}
		});

	let canonical_alias = self
		.services
		.state_accessor
		.get_canonical_alias(room_id)
		.await;

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

	let mut servers = room_mods
		.stream()
		.map(ToOwned::to_owned)
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
		info!("Asking {backfill_server} for backfill");
		let response = self
			.services
			.sending
			.send_federation_request(
				backfill_server,
				federation::backfill::get_backfill::v1::Request {
					room_id: room_id.to_owned(),
					v: vec![first_pdu.1.event_id().to_owned()],
					limit: uint!(100),
				},
			)
			.await;

		match response {
			| Ok(response) => {
				for pdu in response.pdus {
					if let Err(e) = self.backfill_pdu(backfill_server, pdu).await {
						debug_warn!("Failed to add backfilled pdu in room {room_id}: {e}");
					}
				}
				return Ok(());
			},
			| Err(e) => {
				warn!("{backfill_server} failed to provide backfill for room {room_id}: {e}");
			},
		}
	}

	info!("No servers could backfill, but backfill was needed in room {room_id}");
	Ok(())
}

#[implement(super::Service)]
#[tracing::instrument(skip(self, pdu), level = "debug")]
pub async fn backfill_pdu(&self, origin: &ServerName, pdu: Box<RawJsonValue>) -> Result {
	let (room_id, event_id, value) = self
		.services
		.event_handler
		.parse_incoming_pdu(&pdu)
		.await?;

	// Lock so we cannot backfill the same pdu twice at the same time
	let mutex_lock = self
		.services
		.event_handler
		.mutex_federation
		.lock(&room_id)
		.await;

	// Skip the PDU if we already have it as a timeline event
	if let Ok(pdu_id) = self.get_pdu_id(&event_id).await {
		debug!("We already know {event_id} at {pdu_id:?}");
		return Ok(());
	}

	self.services
		.event_handler
		.handle_incoming_pdu(origin, &room_id, &event_id, value, false)
		.boxed()
		.await?;

	let value = self.get_pdu_json(&event_id).await?;

	let pdu = self.get_pdu(&event_id).await?;

	let shortroomid = self
		.services
		.short
		.get_shortroomid(&room_id)
		.await?;

	let insert_lock = self.mutex_insert.lock(&room_id).await;

	let count = self.services.globals.next_count();

	let count: i64 = (*count).try_into()?;
	let pdu_id: RawPduId = PduId {
		shortroomid,
		shorteventid: PduCount::Backfilled(validated!(0 - count)),
	}
	.into();

	// Insert pdu
	self.db
		.prepend_backfill_pdu(&pdu_id, &event_id, &value);

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
