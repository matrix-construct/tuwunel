use std::{borrow::Borrow, collections::HashMap, iter::once, sync::Arc};

use futures::{FutureExt, StreamExt};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedEventId, OwnedServerName, RoomId,
	RoomOrAliasId, RoomVersionId, UserId,
	api::federation::{self, membership::RawStrippedState},
	canonical_json::to_canonical_value,
	events::{
		StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use tuwunel_core::{
	Err, Event, PduCount, Result, at, debug, debug_info, debug_warn, err, extract_variant,
	implement, info,
	matrix::event::gen_event_id,
	pdu::{PduBuilder, PduEvent},
	trace, utils, warn,
};

use super::Service;
use crate::{
	membership::join::get_servers_for_room,
	rooms::{
		state::RoomMutexGuard,
		state_compressor::{CompressedState, HashSetCompressStateEvent},
	},
};

#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(%sender_user, %room_id)
)]
pub async fn knock(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	orig_server_name: Option<&RoomOrAliasId>,
	reason: Option<String>,
	servers: &[OwnedServerName],
	state_lock: &RoomMutexGuard,
) -> Result {
	let servers =
		get_servers_for_room(&self.services, sender_user, room_id, orig_server_name, servers)
			.await?;

	if self
		.services
		.state_cache
		.is_invited(sender_user, room_id)
		.await
	{
		debug_warn!("{sender_user} is already invited in {room_id} but attempted to knock");
		return Err!(Request(Forbidden(
			"You cannot knock on a room you are already invited/accepted to."
		)));
	}

	if self
		.services
		.state_cache
		.is_joined(sender_user, room_id)
		.await
	{
		debug_warn!("{sender_user} is already joined in {room_id} but attempted to knock");
		return Err!(Request(Forbidden("You cannot knock on a room you are already joined in.")));
	}

	if self
		.services
		.state_cache
		.is_knocked(sender_user, room_id)
		.await
	{
		debug_warn!("{sender_user} is already knocked in {room_id}");
		return Ok(());
	}

	if let Ok(membership) = self
		.services
		.state_accessor
		.get_member(room_id, sender_user)
		.await && membership.membership == MembershipState::Ban
	{
		debug_warn!("{sender_user} is banned from {room_id} but attempted to knock");
		return Err!(Request(Forbidden("You cannot knock on a room you are banned from.")));
	}

	let server_in_room = self
		.services
		.state_cache
		.server_in_room(self.services.globals.server_name(), room_id)
		.await;

	let local_knock = server_in_room
		|| servers.is_empty()
		|| (servers.len() == 1 && self.services.globals.server_is_ours(&servers[0]));

	if local_knock {
		self.knock_room_helper_local(sender_user, room_id, reason, &servers, state_lock)
			.boxed()
			.await
	} else {
		self.knock_room_helper_remote(sender_user, room_id, reason, &servers, state_lock)
			.boxed()
			.await
	}
}

#[implement(Service)]
async fn knock_room_helper_local(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
	servers: &[OwnedServerName],
	state_lock: &RoomMutexGuard,
) -> Result {
	debug_info!("We can knock locally");

	let room_version_id = self
		.services
		.state
		.get_room_version(room_id)
		.await?;

	if matches!(
		room_version_id,
		RoomVersionId::V1
			| RoomVersionId::V2
			| RoomVersionId::V3
			| RoomVersionId::V4
			| RoomVersionId::V5
			| RoomVersionId::V6
	) {
		return Err!(Request(Forbidden("This room does not support knocking.")));
	}

	let content = RoomMemberEventContent {
		displayname: self
			.services
			.users
			.displayname(sender_user)
			.await
			.ok(),
		avatar_url: self
			.services
			.users
			.avatar_url(sender_user)
			.await
			.ok(),
		blurhash: self
			.services
			.users
			.blurhash(sender_user)
			.await
			.ok(),
		reason: reason.clone(),
		..RoomMemberEventContent::new(MembershipState::Knock)
	};

	// Try normal knock first
	let Err(error) = self
		.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(sender_user.to_string(), &content),
			sender_user,
			room_id,
			state_lock,
		)
		.await
	else {
		return Ok(());
	};

	if servers.is_empty()
		|| (servers.len() == 1 && self.services.globals.server_is_ours(&servers[0]))
	{
		return Err(error);
	}

	warn!("We couldn't do the knock locally, maybe federation can help to satisfy the knock");

	let (make_knock_response, remote_server) = self
		.make_knock_request(sender_user, room_id, servers)
		.await?;

	info!("make_knock finished");

	let room_version_id = make_knock_response.room_version;

	if !self
		.services
		.config
		.supported_room_version(&room_version_id)
	{
		return Err!(BadServerResponse(
			"Remote room version {room_version_id} is not supported by tuwunel"
		));
	}

	let mut knock_event_stub = serde_json::from_str::<CanonicalJsonObject>(
		make_knock_response.event.get(),
	)
	.map_err(|e| {
		err!(BadServerResponse("Invalid make_knock event json received from server: {e:?}"))
	})?;

	knock_event_stub.insert(
		"origin".into(),
		CanonicalJsonValue::String(
			self.services
				.globals
				.server_name()
				.as_str()
				.to_owned(),
		),
	);
	knock_event_stub.insert(
		"origin_server_ts".into(),
		CanonicalJsonValue::Integer(
			utils::millis_since_unix_epoch()
				.try_into()
				.expect("Timestamp is valid js_int value"),
		),
	);
	knock_event_stub.insert(
		"content".into(),
		to_canonical_value(RoomMemberEventContent {
			displayname: self
				.services
				.users
				.displayname(sender_user)
				.await
				.ok(),
			avatar_url: self
				.services
				.users
				.avatar_url(sender_user)
				.await
				.ok(),
			blurhash: self
				.services
				.users
				.blurhash(sender_user)
				.await
				.ok(),
			reason,
			..RoomMemberEventContent::new(MembershipState::Knock)
		})
		.expect("event is valid, we just created it"),
	);

	knock_event_stub
		.insert("room_id".into(), CanonicalJsonValue::String(room_id.as_str().into()));

	knock_event_stub
		.insert("state_key".into(), CanonicalJsonValue::String(sender_user.as_str().into()));

	knock_event_stub
		.insert("sender".into(), CanonicalJsonValue::String(sender_user.as_str().into()));

	knock_event_stub.insert("type".into(), CanonicalJsonValue::String("m.room.member".into()));

	// In order to create a compatible ref hash (EventID) the `hashes` field needs
	// to be present
	self.services
		.server_keys
		.hash_and_sign_event(&mut knock_event_stub, &room_version_id)?;

	// Generate event id
	let event_id = gen_event_id(&knock_event_stub, &room_version_id)?;

	// Add event_id
	knock_event_stub
		.insert("event_id".into(), CanonicalJsonValue::String(event_id.clone().into()));

	// It has enough fields to be called a proper event now
	let knock_event = knock_event_stub;

	info!("Asking {remote_server} for send_knock in room {room_id}");
	let send_knock_request = federation::membership::create_knock_event::v1::Request {
		room_id: room_id.to_owned(),
		event_id: event_id.clone(),
		pdu: self
			.services
			.federation
			.format_pdu_into(knock_event.clone(), Some(&room_version_id))
			.await,
	};

	let send_knock_response = self
		.services
		.federation
		.execute(&remote_server, send_knock_request)
		.await?;

	info!("send_knock finished");

	self.services
		.short
		.get_or_create_shortroomid(room_id)
		.await;

	info!("Parsing knock event");

	let parsed_knock_pdu = PduEvent::from_id_val(&event_id, knock_event.clone())
		.map_err(|e| err!(BadServerResponse("Invalid knock event PDU: {e:?}")))?;

	info!("Updating membership locally to knock state with provided stripped state events");
	let count = self.services.globals.next_count();
	self.services
		.state_cache
		.update_membership(
			room_id,
			sender_user,
			parsed_knock_pdu
				.get_content::<RoomMemberEventContent>()
				.expect("we just created this"),
			sender_user,
			Some(
				send_knock_response
					.knock_room_state
					.into_iter()
					.filter_map(|s| extract_variant!(s, RawStrippedState::Stripped))
					.collect(),
			),
			None,
			false,
			PduCount::Normal(*count),
		)
		.await?;

	info!("Appending room knock event locally");
	self.services
		.timeline
		.append_pdu(
			&parsed_knock_pdu,
			knock_event,
			once(parsed_knock_pdu.event_id.borrow()),
			state_lock,
		)
		.await?;

	Ok(())
}

#[implement(Service)]
async fn knock_room_helper_remote(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
	servers: &[OwnedServerName],
	state_lock: &RoomMutexGuard,
) -> Result {
	info!("Knocking {room_id} over federation.");

	let (make_knock_response, remote_server) = self
		.make_knock_request(sender_user, room_id, servers)
		.await?;

	info!("make_knock finished");

	let room_version_id = make_knock_response.room_version;

	if !self
		.services
		.config
		.supported_room_version(&room_version_id)
	{
		return Err!(BadServerResponse(
			"Remote room version {room_version_id} is not supported by tuwunel"
		));
	}

	let mut knock_event_stub: CanonicalJsonObject =
		serde_json::from_str(make_knock_response.event.get()).map_err(|e| {
			err!(BadServerResponse("Invalid make_knock event json received from server: {e:?}"))
		})?;

	knock_event_stub.insert(
		"origin".into(),
		CanonicalJsonValue::String(
			self.services
				.globals
				.server_name()
				.as_str()
				.to_owned(),
		),
	);
	knock_event_stub.insert(
		"origin_server_ts".into(),
		CanonicalJsonValue::Integer(
			utils::millis_since_unix_epoch()
				.try_into()
				.expect("Timestamp is valid js_int value"),
		),
	);
	knock_event_stub.insert(
		"content".into(),
		to_canonical_value(RoomMemberEventContent {
			displayname: self
				.services
				.users
				.displayname(sender_user)
				.await
				.ok(),
			avatar_url: self
				.services
				.users
				.avatar_url(sender_user)
				.await
				.ok(),
			blurhash: self
				.services
				.users
				.blurhash(sender_user)
				.await
				.ok(),
			reason,
			..RoomMemberEventContent::new(MembershipState::Knock)
		})
		.expect("event is valid, we just created it"),
	);

	knock_event_stub
		.insert("room_id".into(), CanonicalJsonValue::String(room_id.as_str().into()));

	knock_event_stub
		.insert("state_key".into(), CanonicalJsonValue::String(sender_user.as_str().into()));

	knock_event_stub
		.insert("sender".into(), CanonicalJsonValue::String(sender_user.as_str().into()));

	knock_event_stub.insert("type".into(), CanonicalJsonValue::String("m.room.member".into()));

	// In order to create a compatible ref hash (EventID) the `hashes` field needs
	// to be present
	self.services
		.server_keys
		.hash_and_sign_event(&mut knock_event_stub, &room_version_id)?;

	// Generate event id
	let event_id = gen_event_id(&knock_event_stub, &room_version_id)?;

	// Add event_id
	knock_event_stub
		.insert("event_id".into(), CanonicalJsonValue::String(event_id.clone().into()));

	// It has enough fields to be called a proper event now
	let knock_event = knock_event_stub;

	info!("Asking {remote_server} for send_knock in room {room_id}");
	let send_knock_request = federation::membership::create_knock_event::v1::Request {
		room_id: room_id.to_owned(),
		event_id: event_id.clone(),
		pdu: self
			.services
			.federation
			.format_pdu_into(knock_event.clone(), Some(&room_version_id))
			.await,
	};

	let send_knock_response = self
		.services
		.federation
		.execute(&remote_server, send_knock_request)
		.await?;

	info!("send_knock finished");

	self.services
		.short
		.get_or_create_shortroomid(room_id)
		.await;

	info!("Parsing knock event");
	let parsed_knock_pdu = PduEvent::from_id_val(&event_id, knock_event.clone())
		.map_err(|e| err!(BadServerResponse("Invalid knock event PDU: {e:?}")))?;

	info!("Going through send_knock response knock state events");
	let state = send_knock_response
		.knock_room_state
		.iter()
		.map(|event| {
			serde_json::from_str::<CanonicalJsonObject>(
				extract_variant!(event.clone(), RawStrippedState::Stripped)
					.expect("Raw<AnyStrippedStateEvent>")
					.json()
					.get(),
			)
		})
		.filter_map(Result::ok);

	let mut state_map: HashMap<u64, OwnedEventId> = HashMap::new();

	for event in state {
		let Some(state_key) = event.get("state_key") else {
			debug_warn!("send_knock stripped state event missing state_key: {event:?}");
			continue;
		};
		let Some(event_type) = event.get("type") else {
			debug_warn!("send_knock stripped state event missing event type: {event:?}");
			continue;
		};

		let Ok(state_key) = serde_json::from_value::<String>(state_key.clone().into()) else {
			debug_warn!("send_knock stripped state event has invalid state_key: {event:?}");
			continue;
		};
		let Ok(event_type) = serde_json::from_value::<StateEventType>(event_type.clone().into())
		else {
			debug_warn!("send_knock stripped state event has invalid event type: {event:?}");
			continue;
		};

		let event_id = gen_event_id(&event, &room_version_id)?;
		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&event_type, &state_key)
			.await;

		self.services
			.timeline
			.add_pdu_outlier(&event_id, &event);

		state_map.insert(shortstatekey, event_id.clone());
	}

	info!("Compressing state from send_knock");
	let compressed: CompressedState = self
		.services
		.state_compressor
		.compress_state_events(
			state_map
				.iter()
				.map(|(ssk, eid)| (ssk, eid.borrow())),
		)
		.collect()
		.await;

	debug!("Saving compressed state");
	let HashSetCompressStateEvent {
		shortstatehash: statehash_before_knock,
		added,
		removed,
	} = self
		.services
		.state_compressor
		.save_state(room_id, Arc::new(compressed))
		.await?;

	debug!("Forcing state for new room");
	self.services
		.state
		.force_state(room_id, statehash_before_knock, added, removed, state_lock)
		.await?;

	let statehash_after_knock = self
		.services
		.state
		.append_to_state(&parsed_knock_pdu)
		.await?;

	info!("Updating membership locally to knock state with provided stripped state events");
	let count = self.services.globals.next_count();
	self.services
		.state_cache
		.update_membership(
			room_id,
			sender_user,
			parsed_knock_pdu
				.get_content::<RoomMemberEventContent>()
				.expect("we just created this"),
			sender_user,
			Some(
				send_knock_response
					.knock_room_state
					.into_iter()
					.filter_map(|s| extract_variant!(s, RawStrippedState::Stripped))
					.collect(),
			),
			None,
			false,
			PduCount::Normal(*count),
		)
		.await?;

	info!("Appending room knock event locally");
	self.services
		.timeline
		.append_pdu(
			&parsed_knock_pdu,
			knock_event,
			once(parsed_knock_pdu.event_id.borrow()),
			state_lock,
		)
		.await?;

	info!("Setting final room state for new room");
	// We set the room state after inserting the pdu, so that we never have a moment
	// in time where events in the current room state do not exist
	self.services
		.state
		.set_room_state(room_id, statehash_after_knock, state_lock);

	Ok(())
}

#[implement(Service)]
async fn make_knock_request(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	servers: &[OwnedServerName],
) -> Result<(federation::membership::prepare_knock_event::v1::Response, OwnedServerName)> {
	let mut make_knock_response_and_server =
		Err!(BadServerResponse("No server available to assist in knocking."));

	let mut make_knock_counter: usize = 0;

	for remote_server in servers {
		if self
			.services
			.globals
			.server_is_ours(remote_server)
		{
			continue;
		}

		info!("Asking {remote_server} for make_knock ({make_knock_counter})");

		let make_knock_response = self
			.services
			.federation
			.execute(remote_server, federation::membership::prepare_knock_event::v1::Request {
				room_id: room_id.to_owned(),
				user_id: sender_user.to_owned(),
				ver: self
					.services
					.config
					.supported_room_versions()
					.map(at!(0))
					.collect(),
			})
			.await;

		trace!("make_knock response: {make_knock_response:?}");
		make_knock_counter = make_knock_counter.saturating_add(1);

		make_knock_response_and_server = make_knock_response.map(|r| (r, remote_server.clone()));

		if make_knock_response_and_server.is_ok() {
			break;
		}

		if make_knock_counter > 40 {
			warn!(
				"50 servers failed to provide valid make_knock response, assuming no server can \
				 assist in knocking."
			);
			make_knock_response_and_server =
				Err!(BadServerResponse("No server available to assist in knocking."));

			return make_knock_response_and_server;
		}
	}

	make_knock_response_and_server
}
