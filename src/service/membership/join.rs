use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	iter::once,
	sync::Arc,
};

use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, pin_mut};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedServerName, OwnedUserId, RoomId, RoomOrAliasId,
	RoomVersionId, UserId,
	api::{client::error::ErrorKind, federation},
	canonical_json::to_canonical_value,
	events::{
		StateEventType,
		room::{
			join_rules::RoomJoinRulesEventContent,
			member::{MembershipState, RoomMemberEventContent},
		},
	},
	room::{AllowRule, JoinRule},
};
use tuwunel_core::{
	Err, Result, debug, debug_error, debug_info, debug_warn, err, error, implement, info,
	matrix::{event::gen_event_id_canonical_json, room_version},
	pdu::{PduBuilder, format::from_incoming_federation},
	state_res, trace,
	utils::{self, IterStream, ReadyExt, shuffle},
	warn,
};

use super::Service;
use crate::{
	Services,
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
#[allow(clippy::too_many_arguments)]
pub async fn join(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	orig_room_id: Option<&RoomOrAliasId>,
	reason: Option<String>,
	servers: &[OwnedServerName],
	is_appservice: bool,
	state_lock: &RoomMutexGuard,
) -> Result {
	let servers =
		get_servers_for_room(&self.services, sender_user, room_id, orig_room_id, servers).await?;

	let user_is_guest = self
		.services
		.users
		.is_deactivated(sender_user)
		.await
		.unwrap_or(false)
		&& !is_appservice;

	if user_is_guest
		&& !self
			.services
			.state_accessor
			.guest_can_join(room_id)
			.await
	{
		return Err!(Request(Forbidden("Guests are not allowed to join this room")));
	}

	if self
		.services
		.state_cache
		.is_joined(sender_user, room_id)
		.await
	{
		debug_warn!("{sender_user} is already joined in {room_id}");
		return Ok(());
	}

	if let Ok(membership) = self
		.services
		.state_accessor
		.get_member(room_id, sender_user)
		.await
	{
		if membership.membership == MembershipState::Ban {
			debug_warn!("{sender_user} is banned from {room_id} but attempted to join");
			return Err!(Request(Forbidden("You are banned from the room.")));
		}
	}

	let server_in_room = self
		.services
		.state_cache
		.server_in_room(self.services.globals.server_name(), room_id)
		.await;

	let local_join = server_in_room
		|| servers.is_empty()
		|| (servers.len() == 1 && self.services.globals.server_is_ours(&servers[0]));

	if local_join {
		self.join_local(sender_user, room_id, reason, &servers, state_lock)
			.boxed()
			.await?;
	} else {
		// Ask a remote server if we are not participating in this room
		self.join_remote(sender_user, room_id, reason, &servers, state_lock)
			.boxed()
			.await?;
	}

	Ok(())
}

#[implement(Service)]
#[tracing::instrument(
	name = "remote",
	level = "debug",
	skip_all,
	fields(?servers)
)]
pub async fn join_remote(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
	servers: &[OwnedServerName],
	state_lock: &RoomMutexGuard,
) -> Result {
	info!("Joining {room_id} over federation.");

	let (make_join_response, remote_server) = self
		.make_join_request(sender_user, room_id, servers)
		.await?;

	info!("make_join finished");

	let Some(room_version_id) = make_join_response.room_version else {
		return Err!(BadServerResponse("Remote room version is not supported by tuwunel"));
	};

	if !self
		.services
		.server
		.supported_room_version(&room_version_id)
	{
		return Err!(BadServerResponse(
			"Remote room version {room_version_id} is not supported by tuwunel"
		));
	}

	let room_version_rules = room_version::rules(&room_version_id)?;

	let mut join_event_stub: CanonicalJsonObject =
		serde_json::from_str(make_join_response.event.get()).map_err(|e| {
			err!(BadServerResponse(warn!(
				"Invalid make_join event json received from server: {e:?}"
			)))
		})?;

	let join_authorized_via_users_server = {
		use RoomVersionId::*;
		if !matches!(room_version_id, V1 | V2 | V3 | V4 | V5 | V6 | V7) {
			join_event_stub
				.get("content")
				.map(|s| {
					s.as_object()?
						.get("join_authorised_via_users_server")?
						.as_str()
				})
				.and_then(|s| OwnedUserId::try_from(s.unwrap_or_default()).ok())
		} else {
			None
		}
	};

	join_event_stub.insert(
		"origin".into(),
		CanonicalJsonValue::String(
			self.services
				.globals
				.server_name()
				.as_str()
				.to_owned(),
		),
	);
	join_event_stub.insert(
		"origin_server_ts".into(),
		CanonicalJsonValue::Integer(
			utils::millis_since_unix_epoch()
				.try_into()
				.expect("Timestamp is valid js_int value"),
		),
	);
	join_event_stub.insert(
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
			join_authorized_via_users_server: join_authorized_via_users_server.clone(),
			..RoomMemberEventContent::new(MembershipState::Join)
		})
		.expect("event is valid, we just created it"),
	);

	let event_id = self
		.services
		.server_keys
		.gen_id_hash_and_sign_event(&mut join_event_stub, &room_version_id)?;

	// It has enough fields to be called a proper event now
	let mut join_event = join_event_stub;

	info!("Asking {remote_server} for send_join in room {room_id}");
	let send_join_request = federation::membership::create_join_event::v2::Request {
		room_id: room_id.to_owned(),
		event_id: event_id.clone(),
		omit_members: false,
		pdu: self
			.services
			.federation
			.format_pdu_into(join_event.clone(), Some(&room_version_id))
			.await,
	};

	// Once send_join hits the remote server it may start sending us events which
	// have to be belayed until we process this response first.
	let _federation_lock = self
		.services
		.event_handler
		.mutex_federation
		.lock(room_id)
		.await;

	let send_join_response = match self
		.services
		.federation
		.execute(&remote_server, send_join_request)
		.await
	{
		| Ok(response) => response,
		| Err(e) => {
			error!("send_join failed: {e}");
			return Err(e);
		},
	};

	info!("send_join finished");

	if join_authorized_via_users_server.is_some() {
		if let Some(signed_raw) = &send_join_response.room_state.event {
			debug_info!(
				"There is a signed event with join_authorized_via_users_server. This room is \
				 probably using restricted joins. Adding signature to our event"
			);

			let (signed_event_id, signed_value) =
				gen_event_id_canonical_json(signed_raw, &room_version_id).map_err(|e| {
					err!(Request(BadJson(warn!(
						"Could not convert event to canonical JSON: {e}"
					))))
				})?;

			if signed_event_id != event_id {
				return Err!(Request(BadJson(warn!(
					%signed_event_id, %event_id,
					"Server {remote_server} sent event with wrong event ID"
				))));
			}

			match signed_value["signatures"]
				.as_object()
				.ok_or_else(|| {
					err!(BadServerResponse(warn!(
						"Server {remote_server} sent invalid signatures type"
					)))
				})
				.and_then(|e| {
					e.get(remote_server.as_str()).ok_or_else(|| {
						err!(BadServerResponse(warn!(
							"Server {remote_server} did not send its signature for a restricted \
							 room"
						)))
					})
				}) {
				| Ok(signature) => {
					join_event
						.get_mut("signatures")
						.expect("we created a valid pdu")
						.as_object_mut()
						.expect("we created a valid pdu")
						.insert(remote_server.as_str().into(), signature.clone());
				},
				| Err(e) => {
					warn!(
						"Server {remote_server} sent invalid signature in send_join signatures \
						 for event {signed_value:?}: {e:?}",
					);
				},
			}
		}
	}

	self.services
		.short
		.get_or_create_shortroomid(room_id)
		.await;

	info!("Parsing join event");
	let parsed_join_pdu =
		from_incoming_federation(room_id, &event_id, &mut join_event, &room_version_rules)?;

	info!("Acquiring server signing keys for response events");
	let resp_events = &send_join_response.room_state;
	let resp_state = &resp_events.state;
	let resp_auth = &resp_events.auth_chain;
	self.services
		.server_keys
		.acquire_events_pubkeys(resp_auth.iter().chain(resp_state.iter()))
		.await;

	info!("Going through send_join response room_state");
	let cork = self.services.db.cork_and_flush();
	let state = send_join_response
		.room_state
		.state
		.iter()
		.stream()
		.then(|pdu| {
			self.services
				.server_keys
				.validate_and_add_event_id_no_fetch(pdu, &room_version_id)
		})
		.inspect_err(|e| debug_error!("Invalid send_join state event: {e:?}"))
		.ready_filter_map(Result::ok)
		.ready_filter_map(|(event_id, mut value)| {
			from_incoming_federation(room_id, &event_id, &mut value, &room_version_rules)
				.inspect_err(|e| {
					debug_warn!("Invalid PDU in send_join response: {e:?}: {value:#?}");
				})
				.map(move |pdu| (event_id, pdu, value))
				.ok()
		})
		.fold(HashMap::new(), async |mut state, (event_id, pdu, value)| {
			self.services
				.timeline
				.add_pdu_outlier(&event_id, &value);

			if let Some(state_key) = &pdu.state_key {
				let shortstatekey = self
					.services
					.short
					.get_or_create_shortstatekey(&pdu.kind.to_string().into(), state_key)
					.await;

				state.insert(shortstatekey, pdu.event_id.clone());
			}

			state
		})
		.await;

	drop(cork);

	info!("Going through send_join response auth_chain");
	let cork = self.services.db.cork_and_flush();
	send_join_response
		.room_state
		.auth_chain
		.iter()
		.stream()
		.then(|pdu| {
			self.services
				.server_keys
				.validate_and_add_event_id_no_fetch(pdu, &room_version_id)
		})
		.inspect_err(|e| debug_error!("Invalid send_join auth_chain event: {e:?}"))
		.ready_filter_map(Result::ok)
		.ready_for_each(|(event_id, mut value)| {
			if !room_version_rules
				.event_format
				.require_room_create_room_id
				&& value["type"] == "m.room.create"
			{
				let room_id = CanonicalJsonValue::String(room_id.as_str().into());
				value.insert("room_id".into(), room_id);
			}

			self.services
				.timeline
				.add_pdu_outlier(&event_id, &value);
		})
		.await;

	drop(cork);

	debug!("Running send_join auth check");
	state_res::auth_check(
		&room_version_rules,
		&parsed_join_pdu,
		&async |event_id| self.services.timeline.get_pdu(&event_id).await,
		&async |event_type, state_key| {
			let shortstatekey = self
				.services
				.short
				.get_shortstatekey(&event_type, state_key.as_str())
				.await?;

			let event_id = state.get(&shortstatekey).ok_or_else(|| {
				err!(Request(NotFound("Missing fetch_state {shortstatekey:?}")))
			})?;

			self.services.timeline.get_pdu(event_id).await
		},
	)
	.inspect_err(|e| error!("send_join auth check failed: {e:?}"))
	.boxed()
	.await?;

	info!("Compressing state from send_join");
	let compressed: CompressedState = self
		.services
		.state_compressor
		.compress_state_events(state.iter().map(|(ssk, eid)| (ssk, eid.borrow())))
		.collect()
		.await;

	debug!("Saving compressed state");
	let HashSetCompressStateEvent {
		shortstatehash: statehash_before_join,
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
		.force_state(room_id, statehash_before_join, added, removed, state_lock)
		.await?;

	info!("Updating joined counts for new room");
	self.services
		.state_cache
		.update_joined_count(room_id)
		.await;

	// We append to state before appending the pdu, so we don't have a moment in
	// time with the pdu without it's state. This is okay because append_pdu can't
	// fail.
	let statehash_after_join = self
		.services
		.state
		.append_to_state(&parsed_join_pdu)
		.await?;

	info!("Appending new room join event");
	self.services
		.timeline
		.append_pdu(
			&parsed_join_pdu,
			join_event,
			once(parsed_join_pdu.event_id.borrow()),
			state_lock,
		)
		.await?;

	info!("Setting final room state for new room");
	// We set the room state after inserting the pdu, so that we never have a moment
	// in time where events in the current room state do not exist
	self.services
		.state
		.set_room_state(room_id, statehash_after_join, state_lock);

	Ok(())
}

#[implement(Service)]
#[tracing::instrument(name = "local", level = "debug", skip_all)]
pub async fn join_local(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
	servers: &[OwnedServerName],
	state_lock: &RoomMutexGuard,
) -> Result {
	debug_info!("We can join locally");

	let join_rules_event_content = self
		.services
		.state_accessor
		.room_state_get_content::<RoomJoinRulesEventContent>(
			room_id,
			&StateEventType::RoomJoinRules,
			"",
		)
		.await;

	let restriction_rooms = match join_rules_event_content {
		| Ok(RoomJoinRulesEventContent {
			join_rule: JoinRule::Restricted(restricted) | JoinRule::KnockRestricted(restricted),
		}) => restricted
			.allow
			.into_iter()
			.filter_map(|a| match a {
				| AllowRule::RoomMembership(r) => Some(r.room_id),
				| _ => None,
			})
			.collect(),
		| _ => Vec::new(),
	};

	let join_authorized_via_users_server: Option<OwnedUserId> = {
		if restriction_rooms
			.iter()
			.stream()
			.any(|restriction_room_id| {
				self.services
					.state_cache
					.is_joined(sender_user, restriction_room_id)
			})
			.await
		{
			let users = self
				.services
				.state_cache
				.local_users_in_room(room_id)
				.filter(|user| {
					self.services.state_accessor.user_can_invite(
						room_id,
						user,
						sender_user,
						state_lock,
					)
				})
				.map(ToOwned::to_owned);

			pin_mut!(users);
			users.next().await
		} else {
			None
		}
	};

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
		join_authorized_via_users_server,
		..RoomMemberEventContent::new(MembershipState::Join)
	};

	// Try normal join first
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

	if restriction_rooms.is_empty()
		&& (servers.is_empty()
			|| servers.len() == 1 && self.services.globals.server_is_ours(&servers[0]))
	{
		return Err(error);
	}

	warn!(
		"We couldn't do the join locally, maybe federation can help to satisfy the restricted \
		 join requirements"
	);
	let Ok((make_join_response, remote_server)) = self
		.make_join_request(sender_user, room_id, servers)
		.await
	else {
		return Err(error);
	};

	let Some(room_version_id) = make_join_response.room_version else {
		return Err!(BadServerResponse("Remote room version is not supported by tuwunel"));
	};

	if !self
		.services
		.server
		.supported_room_version(&room_version_id)
	{
		return Err!(BadServerResponse(
			"Remote room version {room_version_id} is not supported by tuwunel"
		));
	}

	let mut join_event_stub: CanonicalJsonObject =
		serde_json::from_str(make_join_response.event.get()).map_err(|e| {
			err!(BadServerResponse("Invalid make_join event json received from server: {e:?}"))
		})?;

	let join_authorized_via_users_server = join_event_stub
		.get("content")
		.map(|s| {
			s.as_object()?
				.get("join_authorised_via_users_server")?
				.as_str()
		})
		.and_then(|s| OwnedUserId::try_from(s.unwrap_or_default()).ok());

	join_event_stub.insert(
		"origin".into(),
		CanonicalJsonValue::String(
			self.services
				.globals
				.server_name()
				.as_str()
				.to_owned(),
		),
	);
	join_event_stub.insert(
		"origin_server_ts".into(),
		CanonicalJsonValue::Integer(
			utils::millis_since_unix_epoch()
				.try_into()
				.expect("Timestamp is valid js_int value"),
		),
	);
	join_event_stub.insert(
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
			join_authorized_via_users_server,
			..RoomMemberEventContent::new(MembershipState::Join)
		})
		.expect("event is valid, we just created it"),
	);

	let event_id = self
		.services
		.server_keys
		.gen_id_hash_and_sign_event(&mut join_event_stub, &room_version_id)?;

	// It has enough fields to be called a proper event now
	let join_event = join_event_stub;

	let send_join_response = self
		.services
		.federation
		.execute(&remote_server, federation::membership::create_join_event::v2::Request {
			room_id: room_id.to_owned(),
			event_id: event_id.clone(),
			omit_members: false,
			pdu: self
				.services
				.federation
				.format_pdu_into(join_event.clone(), Some(&room_version_id))
				.await,
		})
		.await?;

	if let Some(signed_raw) = send_join_response.room_state.event {
		let (signed_event_id, signed_value) =
			gen_event_id_canonical_json(&signed_raw, &room_version_id).map_err(|e| {
				err!(Request(BadJson(warn!("Could not convert event to canonical JSON: {e}"))))
			})?;

		if signed_event_id != event_id {
			return Err!(Request(BadJson(
				warn!(%signed_event_id, %event_id, "Server {remote_server} sent event with wrong event ID")
			)));
		}

		self.services
			.event_handler
			.handle_incoming_pdu(&remote_server, room_id, &signed_event_id, signed_value, true)
			.boxed()
			.await?;
	} else {
		return Err(error);
	}

	Ok(())
}

#[implement(Service)]
#[tracing::instrument(
	name = "make_join",
	level = "debug",
	skip_all,
	fields(?servers)
)]
async fn make_join_request(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	servers: &[OwnedServerName],
) -> Result<(federation::membership::prepare_join_event::v1::Response, OwnedServerName)> {
	let mut make_join_response_and_server =
		Err!(BadServerResponse("No server available to assist in joining."));

	let mut make_join_counter: usize = 0;
	let mut incompatible_room_version_count: usize = 0;

	for remote_server in servers {
		if self
			.services
			.globals
			.server_is_ours(remote_server)
		{
			continue;
		}
		info!("Asking {remote_server} for make_join ({make_join_counter})");
		let make_join_response = self
			.services
			.federation
			.execute(remote_server, federation::membership::prepare_join_event::v1::Request {
				room_id: room_id.to_owned(),
				user_id: sender_user.to_owned(),
				ver: self
					.services
					.server
					.supported_room_versions()
					.collect(),
			})
			.await;

		trace!("make_join response: {make_join_response:?}");
		make_join_counter = make_join_counter.saturating_add(1);

		if let Err(ref e) = make_join_response {
			if matches!(
				e.kind(),
				ErrorKind::IncompatibleRoomVersion { .. } | ErrorKind::UnsupportedRoomVersion
			) {
				incompatible_room_version_count =
					incompatible_room_version_count.saturating_add(1);
			}

			if incompatible_room_version_count > 15 {
				info!(
					"15 servers have responded with M_INCOMPATIBLE_ROOM_VERSION or \
					 M_UNSUPPORTED_ROOM_VERSION, assuming that tuwunel does not support the \
					 room version {room_id}: {e}"
				);
				make_join_response_and_server =
					Err!(BadServerResponse("Room version is not supported by tuwunel"));
				return make_join_response_and_server;
			}

			let max_attempts = self
				.services
				.config
				.max_make_join_attempts_per_join_attempt;
			if make_join_counter >= max_attempts {
				warn!(?remote_server, "last make_join failure reason: {e}");
				warn!(
					"{max_attempts} servers failed to provide valid make_join response, \
					 assuming no server can assist in joining."
				);
				make_join_response_and_server =
					Err!(BadServerResponse("No server available to assist in joining."));

				return make_join_response_and_server;
			}
		}

		make_join_response_and_server = make_join_response.map(|r| (r, remote_server.clone()));

		if make_join_response_and_server.is_ok() {
			break;
		}
	}

	make_join_response_and_server
}

pub(super) async fn get_servers_for_room(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	orig_room_id: Option<&RoomOrAliasId>,
	via: &[OwnedServerName],
) -> Result<Vec<OwnedServerName>> {
	// add invited vias
	let mut additional_servers = services
		.state_cache
		.servers_invite_via(room_id)
		.map(ToOwned::to_owned)
		.collect::<Vec<_>>()
		.await;

	// add invite senders' servers
	additional_servers.extend(
		services
			.state_cache
			.invite_state(user_id, room_id)
			.await
			.unwrap_or_default()
			.iter()
			.filter_map(|event| event.get_field("sender").ok().flatten())
			.filter_map(|sender: &str| UserId::parse(sender).ok())
			.map(|user| user.server_name().to_owned()),
	);

	let mut servers = Vec::from(via);
	shuffle(&mut servers);

	if let Some(server_name) = room_id.server_name() {
		servers.insert(0, server_name.to_owned());
	}

	if let Some(orig_room_id) = orig_room_id
		&& let Some(orig_server_name) = orig_room_id.server_name()
	{
		servers.insert(0, orig_server_name.to_owned());
	}

	shuffle(&mut additional_servers);

	servers.extend_from_slice(&additional_servers);

	// 1. (room alias server)?
	// 2. (room id server)?
	// 3. shuffle [via query + resolve servers]?
	// 4. shuffle [invited via, inviters servers]?

	info!("{servers:?}");

	// dedup preserving order
	let mut set = HashSet::new();
	servers.retain(|x| set.insert(x.clone()));

	info!("{servers:?}");

	// sort deprioritized servers last
	if !servers.is_empty() {
		for i in 0..servers.len() {
			if services
				.server
				.config
				.deprioritize_joins_through_servers
				.is_match(servers[i].host())
			{
				let server = servers.remove(i);
				servers.push(server);
			}
		}
	}

	Ok(servers)
}
