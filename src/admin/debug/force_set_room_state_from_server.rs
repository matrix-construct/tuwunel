use std::collections::HashMap;

use futures::TryStreamExt;
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, OwnedRoomId, OwnedServerName, RoomId,
	api::federation::event::get_room_state,
};
use tuwunel_core::{
	Err, Result, err, info,
	matrix::{
		Event, RoomVersionRules,
		pdu::{PduEvent, from_incoming_federation},
		room_version::rules as room_version_rules,
	},
	utils::{
		IterStream, ReadyExt,
		stream::{BroadbandExt, WidebandExt},
	},
	warn,
};
use tuwunel_service::{Services, rooms::state_compressor::HashSetCompressStateEvent};

use crate::admin_command;

#[admin_command]
#[tracing::instrument(level = "debug", skip(self))]
pub(super) async fn force_set_room_state_from_server(
	&self,
	room_id: OwnedRoomId,
	server_name: OwnedServerName,
) -> Result {
	// TODO: diverged from join remote

	if !self
		.services
		.state_cache
		.server_in_room(&self.services.server.name, &room_id)
		.await
	{
		return Err!("We are not participating in the room / we don't know about the room ID.");
	}

	let first_pdu = self
		.services
		.timeline
		.latest_pdu_in_room(&room_id)
		.await
		.map_err(|_| err!(Database("Failed to find the latest PDU in database")))?;

	let room_version = self
		.services
		.state
		.get_room_version(&room_id)
		.await?;

	let rules = room_version_rules(&room_version)?;

	let remote_state_response = self
		.services
		.federation
		.execute(&server_name, get_room_state::v1::Request {
			room_id: room_id.clone(),
			event_id: first_pdu.event_id().to_owned(),
		})
		.await?;

	for pdu in remote_state_response.pdus.clone() {
		match self
			.services
			.event_handler
			.parse_incoming_pdu(&pdu)
			.await
		{
			| Ok(t) => t,
			| Err(e) => {
				warn!("Could not parse PDU, ignoring: {e}");
				continue;
			},
		};
	}

	info!("Acquiring server signing keys for response events");
	self.services
		.server_keys
		.acquire_events_pubkeys(
			remote_state_response
				.auth_chain
				.iter()
				.chain(remote_state_response.pdus.iter()),
		)
		.await;

	let validate = |pdu| {
		self.services
			.server_keys
			.validate_and_add_event_id_no_fetch(pdu, &room_version)
	};

	info!("Going through room_state response PDUs");
	let state: HashMap<u64, OwnedEventId> = remote_state_response
		.pdus
		.iter()
		.stream()
		.wide_filter_map(async |pdu| {
			let (event_id, value) = validate(pdu).await.ok()?;

			ingest_state_pdu(self.services, &room_id, &event_id, value, &rules)
				.await
				.transpose()
		})
		.try_collect()
		.await?;

	info!("Going through auth_chain response");
	remote_state_response
		.auth_chain
		.iter()
		.stream()
		.broad_then(|pdu| validate(pdu))
		.ready_filter_map(Result::ok)
		.ready_for_each(|(event_id, value)| {
			let value = from_incoming_federation(&room_id, &event_id, value, &rules);

			self.services
				.timeline
				.add_pdu_outlier(&event_id, &value);
		})
		.await;

	let new_room_state = self
		.services
		.event_handler
		.resolve_state(&room_id, &room_version, state)
		.await?;

	info!("Forcing new room state");
	let HashSetCompressStateEvent {
		shortstatehash: short_state_hash,
		added,
		removed,
	} = self
		.services
		.state_compressor
		.save_state(room_id.clone().as_ref(), new_room_state)
		.await?;

	let state_lock = self.services.state.mutex.lock(&*room_id).await;

	self.services
		.state
		.force_state(room_id.clone().as_ref(), short_state_hash, added, removed, &state_lock)
		.await?;

	info!(
		"Updating joined counts for room just in case (e.g. we may have found a difference in \
		 the room's m.room.member state"
	);
	self.services
		.state_cache
		.update_joined_count(&room_id)
		.await;

	self.write_str("Successfully forced the room state from the requested remote server.")
		.await
}

async fn ingest_state_pdu(
	services: &Services,
	room_id: &RoomId,
	event_id: &EventId,
	value: CanonicalJsonObject,
	rules: &RoomVersionRules,
) -> Result<Option<(u64, OwnedEventId)>> {
	let invalid_pdu_err = |e| {
		err!(BadServerResponse(debug_error!(
			"Invalid PDU {event_id} in fetching remote room state PDUs response: {e:?}"
		)))
	};

	let (pdu, value) = PduEvent::from_object_federation(room_id, event_id, value, rules)
		.map_err(invalid_pdu_err)?;

	services
		.timeline
		.add_pdu_outlier(event_id, &value);

	let Some(state_key) = &pdu.state_key else {
		return Ok(None);
	};

	let shortstatekey = services
		.short
		.get_or_create_shortstatekey(&pdu.kind.to_string().into(), state_key)
		.await;

	Ok(Some((shortstatekey, pdu.event_id)))
}
