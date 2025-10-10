use std::iter::once;

use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join, try_join, try_join4},
};
use ruma::{
	CanonicalJsonObject, EventId, RoomId, ServerName,
	api::federation,
	events::{
		StateEventType, TimelineEventType,
		room::{
			power_levels::RoomPowerLevelsEventContent,
				message::{RoomMessageEventContent, MessageType},
			redaction::RoomRedactionEventContent,
		},
	},
	uint,
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{
	Result, debug, debug_info, debug_warn, implement, is_false,
	matrix::{
		event::Event,
		pdu::{PduCount, PduId, RawPduId},
	},
	utils::{
		IterStream, ReadyExt,
		future::{BoolExt, TryExtExt},
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
		.room_state_get_content(room_id, &StateEventType::RoomPowerLevels, "")
		.map_ok(|content: RoomPowerLevelsEventContent| content)
		.unwrap_or_default();

	let (canonical_alias, power_levels) = join(canonical_alias, power_levels).await;

	let room_mods = power_levels
		.users
		.iter()
		.filter_map(|(user_id, level)| {
			(*level > power_levels.users_default && !self.services.globals.user_is_local(user_id))
				.then_some(user_id.server_name())
		});

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
		let request = federation::backfill::get_backfill::v1::Request {
			room_id: room_id.to_owned(),
			v: vec![first_pdu.event_id().to_owned()],
			limit: uint!(100),
		};

		debug_info!("Asking {backfill_server} for backfill");
		if let Ok(response) = self
			.services
			.sending
			.send_federation_request(backfill_server, request)
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

	self.services
		.event_handler
		.handle_incoming_pdu(origin, room_id, &event_id, value, false)
		.boxed()
		.await?;

	let pdu = self.get_pdu(&event_id);

	let value = self.get_pdu_json(&event_id);

	let shortroomid = self.services.short.get_shortroomid(room_id);

	let insert_lock = self.mutex_insert.lock(room_id).map(Ok);

	let (pdu, value, shortroomid, insert_lock) =
		try_join4(pdu, value, shortroomid, insert_lock).await?;

	let count = self.services.globals.next_count();
	let count: i64 = (*count).try_into()?;
	let pdu_id: RawPduId = PduId {
		shortroomid,
		shorteventid: PduCount::Backfilled(validated!(0 - count)),
	}
	.into();

	// Insert pdu
	self.prepend_backfill_pdu(&pdu_id, &event_id, &value);
	drop(insert_lock);

	if pdu.kind == TimelineEventType::RoomMessage {
		let content: ExtractBody = pdu.get_content()?;
		if let Some(body) = content.body {
			self.services
				.search
				.index_pdu(shortroomid, &pdu_id, &body);
		}
		// Retention insertion for backfilled message
		if let Ok(full) = pdu.get_content::<RoomMessageEventContent>() {
			use ruma::events::room::MediaSource;
			let mut mxcs: Vec<(String, bool, String)> = Vec::new();
			let push_plain = |mxcs: &mut Vec<(String, bool, String)>, src: &MediaSource, label: &str| {
				if let MediaSource::Plain(mxc) = src { let s = mxc.to_string(); if s.starts_with("mxc://") { mxcs.push((s, true, label.to_owned())); } }
			};
			match &full.msgtype {
				MessageType::Image(c) => {
					push_plain(&mut mxcs, &c.source, "image.source");
					if let Some(info) = c.info.as_ref() { if let Some(th) = info.thumbnail_source.as_ref() { push_plain(&mut mxcs, th, "image.thumbnail_source"); } }
				},
				MessageType::File(c) => {
					push_plain(&mut mxcs, &c.source, "file.source");
					if let Some(info) = c.info.as_ref() { if let Some(th) = info.thumbnail_source.as_ref() { push_plain(&mut mxcs, th, "file.thumbnail_source"); } }
				},
				MessageType::Video(c) => {
					push_plain(&mut mxcs, &c.source, "video.source");
					if let Some(info) = c.info.as_ref() { if let Some(th) = info.thumbnail_source.as_ref() { push_plain(&mut mxcs, th, "video.thumbnail_source"); } }
				},
				MessageType::Audio(c) => {
					push_plain(&mut mxcs, &c.source, "audio.source");
				},
				_ => {},
			}
			if !mxcs.is_empty() { self.services.media.retention_insert_mxcs_on_event(pdu.event_id().as_str(), pdu.room_id().as_str(), &mxcs); }
		}
	}

	if pdu.kind == TimelineEventType::RoomRedaction {
		if let Ok(red) = pdu.get_content::<RoomRedactionEventContent>() { if let Some(rid) = red.redacts { self.services.media.retention_decrement_on_redaction(rid.as_str()).await; } }
	}
	drop(mutex_lock);

	debug!("Prepended backfill pdu");
	Ok(())
}

#[implement(super::Service)]
fn prepend_backfill_pdu(
	&self,
	pdu_id: &RawPduId,
	event_id: &EventId,
	json: &CanonicalJsonObject,
) {
	self.db.pduid_pdu.raw_put(pdu_id, Json(json));
	self.db.eventid_pduid.insert(event_id, pdu_id);
	self.db.eventid_outlierpdu.remove(event_id);
}
