use std::{
	borrow::Borrow,
	collections::{BTreeSet, HashMap, HashSet},
	iter::once,
	mem::take,
	sync::Arc,
};

use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedEventId, OwnedServerName, OwnedUserId, RoomId,
	RoomOrAliasId, RoomVersionId, UserId,
	api::{error::ErrorKind, federation},
	canonical_json::to_canonical_value,
	events::{
		StateEventType,
		room::{
			create::RoomCreateEventContent,
			join_rules::RoomJoinRulesEventContent,
			member::{MembershipState, RoomMemberEventContent},
		},
	},
	room::{AllowRule, JoinRule},
	room_version_rules::RoomVersionRules,
};
use serde_json::value::{RawValue as RawJsonValue, to_raw_value};
use tuwunel_core::{
	Err, Result, at, debug, debug_error, debug_info, debug_warn, err, error, implement, info,
	matrix::{event::gen_event_id_canonical_json, room_version},
	pdu::{Pdu, PduBuilder, check_rules},
	trace,
	utils::{self, BoolExt, IterStream, ReadyExt, math::Expected, shuffle},
	warn,
};

use super::Service;
use crate::{
	Services,
	federation::{Candidates, WhenAllBackedOff},
	rooms::{
		state::RoomMutexGuard,
		state_compressor::{CompressedState, HashSetCompressStateEvent},
		state_res,
	},
};

#[implement(Service)]
#[expect(clippy::too_many_arguments)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(%sender_user, %room_id)
)]
pub async fn join(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	orig_room_id: Option<&RoomOrAliasId>,
	reason: Option<String>,
	servers: &[OwnedServerName],
	is_appservice: bool,
	extra_content: Option<CanonicalJsonObject>,
) -> Result {
	let state_lock = self.services.state.mutex.lock(room_id).await;

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

	// Resolved state can lag a federated re-invite; trust the invite index.
	if let Ok(membership) = self
		.services
		.state_accessor
		.get_member(room_id, sender_user)
		.await && membership.membership == MembershipState::Ban
		&& !self
			.services
			.state_cache
			.is_invited(sender_user, room_id)
			.await
	{
		debug_warn!("{sender_user} is banned from {room_id} but attempted to join");
		return Err!(Request(Forbidden("You are banned from the room.")));
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
		self.join_local(sender_user, room_id, reason, &servers, state_lock, extra_content)
			.boxed()
			.await?;
	} else {
		// Ask a remote server if we are not participating in this room
		self.join_remote(sender_user, room_id, reason, &servers, state_lock, extra_content)
			.boxed()
			.await?;
	}

	self.copy_predecessor_push_rules(sender_user, room_id)
		.await;

	Ok(())
}

#[implement(Service)]
async fn copy_predecessor_push_rules(&self, user_id: &UserId, room_id: &RoomId) {
	let Ok(create): Result<RoomCreateEventContent> = self
		.services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomCreate, "")
		.await
	else {
		return;
	};

	let Some(predecessor) = create.predecessor else {
		return;
	};

	self.services
		.account_data
		.copy_room_push_rule(user_id, &predecessor.room_id, room_id)
		.await
		.ok();
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
	state_lock: RoomMutexGuard,
	extra_content: Option<CanonicalJsonObject>,
) -> Result {
	info!("Joining {room_id} over federation.");

	let (make_join_response, remote_server) = self
		.make_join_request(sender_user, room_id, servers)
		.await?;

	info!("make_join finished");

	let room_version_id = self.require_supported_remote_room_version(&make_join_response)?;
	let room_version_rules = room_version::rules(&room_version_id)?;
	let (mut join_event, event_id, join_authorized_via_users_server) = self
		.create_join_event(
			room_id,
			sender_user,
			&make_join_response.event,
			&room_version_id,
			&room_version_rules,
			reason,
			extra_content,
		)
		.await?;

	// Once send_join hits the remote server it may start sending us events which
	// have to be belayed until we process this response first.
	let _federation_lock = self
		.services
		.event_handler
		.mutex_federation
		.lock(room_id)
		.await;

	let mut response = self
		.execute_send_join(
			&remote_server,
			room_id,
			&event_id,
			join_event.clone(),
			&room_version_id,
		)
		.await?;

	if response.members_omitted {
		self.fetch_omitted_state(&remote_server, room_id, &event_id, servers, &mut response)
			.await?;
	}

	if join_authorized_via_users_server.is_some() {
		merge_restricted_signature(
			&remote_server,
			&event_id,
			&room_version_id,
			&response,
			&mut join_event,
		)?;
	}

	let shortroomid = self
		.services
		.short
		.get_or_create_shortroomid(room_id)
		.await;

	info!(
		%room_id,
		%shortroomid,
		"Initialized room. Parsing join event..."
	);
	let (parsed_join_pdu, join_event) =
		Pdu::from_object_federation(room_id, &event_id, join_event, &room_version_rules)?;

	info!(
		events = response
			.state
			.len()
			.expected_add(response.auth_chain.len()),
		"Acquiring server signing keys for response events..."
	);
	self.services
		.server_keys
		.acquire_events_pubkeys(
			response
				.auth_chain
				.iter()
				.chain(response.state.iter()),
		)
		.await;

	let state = self
		.ingest_send_join_state(room_id, &room_version_id, &room_version_rules, &response.state)
		.await;

	self.ingest_send_join_auth_chain(
		room_id,
		&room_version_id,
		&room_version_rules,
		&response.auth_chain,
	)
	.await;

	debug!("Running send_join auth check...");
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

	self.apply_send_join_state(room_id, &state, &state_lock)
		.await?;

	// We append to state before appending the pdu, so we don't have a moment in
	// time with the pdu without it's state. This is okay because append_pdu can't
	// fail.
	let statehash_after_join = self
		.services
		.state
		.append_to_state(&parsed_join_pdu)
		.await?;

	info!(
		event_id = %parsed_join_pdu.event_id,
		"Appending new room join event..."
	);

	self.services
		.timeline
		.append_pdu(
			&parsed_join_pdu,
			join_event,
			once(parsed_join_pdu.event_id.borrow()),
			&state_lock,
		)
		.await?;

	// We set the room state after inserting the pdu, so that we never have a moment
	// in time where events in the current room state do not exist
	self.services
		.state
		.set_room_state(room_id, statehash_after_join, &state_lock);

	info!(
		statehash = %statehash_after_join,
		"Set final room state for new room."
	);

	Ok(())
}

#[implement(Service)]
fn require_supported_remote_room_version(
	&self,
	make_join_response: &federation::membership::prepare_join_event::v1::Response,
) -> Result<RoomVersionId> {
	let Some(room_version_id) = make_join_response.room_version.clone() else {
		return Err!(BadServerResponse("Remote room version is not supported by tuwunel"));
	};

	if !self
		.services
		.config
		.supported_room_version(&room_version_id)
	{
		return Err!(BadServerResponse(
			"Remote room version {room_version_id} is not supported by tuwunel"
		));
	}

	Ok(room_version_id)
}

#[implement(Service)]
async fn execute_send_join(
	&self,
	remote_server: &OwnedServerName,
	room_id: &RoomId,
	event_id: &OwnedEventId,
	join_event: CanonicalJsonObject,
	room_version_id: &RoomVersionId,
) -> Result<federation::membership::create_join_event::v2::RoomState> {
	let send_join_request = federation::membership::create_join_event::v2::Request {
		room_id: room_id.to_owned(),
		event_id: event_id.clone(),
		omit_members: true,
		pdu: self
			.services
			.federation
			.format_pdu_into(join_event, Some(room_version_id))
			.await,
	};

	info!("Asking {remote_server} for fast_join in room {room_id}");
	let response = self
		.services
		.federation
		.execute(remote_server, send_join_request)
		.await
		.inspect_err(|e| error!("send_join failed: {e}"))?
		.room_state;

	info!(
		fast_join = response.members_omitted,
		auth_chain = response.auth_chain.len(),
		state = response.state.len(),
		servers = response
			.servers_in_room
			.as_ref()
			.map(Vec::len)
			.unwrap_or(0),
		"send_join finished"
	);

	Ok(response)
}

#[implement(Service)]
async fn fetch_omitted_state(
	&self,
	remote_server: &OwnedServerName,
	room_id: &RoomId,
	event_id: &OwnedEventId,
	servers: &[OwnedServerName],
	response: &mut federation::membership::create_join_event::v2::RoomState,
) -> Result {
	use federation::event::get_room_state::v1::{Request, Response};

	let eligible =
		self.omitted_state_servers(remote_server, servers, response.servers_in_room.as_deref());

	let candidates = self
		.services
		.federation
		.rank_candidates(eligible, WhenAllBackedOff::Attempt)
		.await;

	let mut last_error = Err!(BadServerResponse("No server provided omitted send_join state."));
	for server in candidates {
		info!("Asking {server} for state in room {room_id}");
		let result = self
			.services
			.federation
			.execute(&server, Request {
				room_id: room_id.to_owned(),
				event_id: event_id.clone(),
			})
			.await;

		match result {
			| Err(e) => {
				debug_warn!(?server, "state fetch failed: {e}");
				last_error = Err(e);
			},
			| Ok(Response { mut auth_chain, mut pdus }) => {
				response.auth_chain = take(&mut auth_chain);
				response.state = take(&mut pdus);

				info!(
					auth_chain = response.auth_chain.len(),
					state = response.state.len(),
					"state finished"
				);

				return Ok(());
			},
		}
	}

	last_error
}

#[implement(Service)]
fn omitted_state_servers(
	&self,
	remote_server: &OwnedServerName,
	servers: &[OwnedServerName],
	servers_in_room: Option<&[String]>,
) -> Candidates {
	let extracted = servers_in_room
		.into_iter()
		.flatten()
		.filter_map(|server| OwnedServerName::parse(server.as_str()).ok());

	let mut seen = BTreeSet::new();
	once(remote_server.clone())
		.chain(extracted)
		.chain(servers.iter().cloned())
		.filter(|server| !self.services.globals.server_is_ours(server))
		.filter(move |server| seen.insert(server.clone()))
		.take(
			self.services
				.config
				.max_make_join_attempts_per_join_attempt,
		)
		.collect()
}

fn merge_restricted_signature(
	remote_server: &OwnedServerName,
	event_id: &OwnedEventId,
	room_version_id: &RoomVersionId,
	response: &federation::membership::create_join_event::v2::RoomState,
	join_event: &mut CanonicalJsonObject,
) -> Result {
	let Some(signed_raw) = &response.event else {
		return Ok(());
	};

	debug_info!(
		"There is a signed event with join_authorized_via_users_server. This room is probably \
		 using restricted joins. Adding signature to our event"
	);

	let (signed_event_id, signed_value) =
		gen_event_id_canonical_json(signed_raw, room_version_id).map_err(|e| {
			err!(Request(BadJson(warn!("Could not convert event to canonical JSON: {e}"))))
		})?;

	if signed_event_id != *event_id {
		return Err!(Request(BadJson(warn!(
			%signed_event_id, %event_id,
			"Server {remote_server} sent event with wrong event ID"
		))));
	}

	let signature = signed_value["signatures"]
		.as_object()
		.ok_or_else(|| {
			err!(BadServerResponse(warn!("Server {remote_server} sent invalid signatures type")))
		})
		.and_then(|e| {
			e.get(remote_server.as_str()).ok_or_else(|| {
				err!(BadServerResponse(warn!(
					"Server {remote_server} did not send its signature for a restricted room"
				)))
			})
		});

	match signature {
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
				"Server {remote_server} sent invalid signature in send_join signatures for \
				 event {signed_value:?}: {e:?}",
			);
		},
	}

	Ok(())
}

#[implement(Service)]
async fn ingest_send_join_state(
	&self,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	room_version_rules: &RoomVersionRules,
	state_pdus: &[Box<RawJsonValue>],
) -> HashMap<u64, OwnedEventId> {
	info!(events = state_pdus.len(), "Going through send_join response room_state...");
	let cork = self.services.db.cork_and_flush();
	let state = state_pdus
		.iter()
		.stream()
		.then(|pdu| {
			self.services
				.server_keys
				.validate_and_add_event_id_no_fetch(pdu, room_version_id)
		})
		.inspect_err(|e| debug_error!("Invalid send_join state event: {e:?}"))
		.ready_filter_map(Result::ok)
		.ready_filter_map(|(event_id, value)| {
			Pdu::from_object_federation(room_id, &event_id, value, room_version_rules)
				.inspect_err(|e| {
					debug_warn!("Invalid PDU {event_id:?} in send_join response: {e:?}");
				})
				.map(move |(pdu, value)| (event_id, pdu, value))
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
	state
}

#[implement(Service)]
async fn ingest_send_join_auth_chain(
	&self,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	room_version_rules: &RoomVersionRules,
	auth_chain: &[Box<RawJsonValue>],
) {
	info!(events = auth_chain.len(), "Going through send_join response auth_chain...");
	let cork = self.services.db.cork_and_flush();
	auth_chain
		.iter()
		.stream()
		.then(|pdu| {
			self.services
				.server_keys
				.validate_and_add_event_id_no_fetch(pdu, room_version_id)
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
}

#[implement(Service)]
async fn apply_send_join_state(
	&self,
	room_id: &RoomId,
	state: &HashMap<u64, OwnedEventId>,
	state_lock: &RoomMutexGuard,
) -> Result {
	info!(events = state.len(), "Compressing state from send_join...");
	let compressed: CompressedState = self
		.services
		.state_compressor
		.compress_state_events(state.iter().map(|(ssk, eid)| (ssk, eid.borrow())))
		.collect()
		.await;

	debug!("Saving compressed state...");
	let HashSetCompressStateEvent {
		shortstatehash: statehash_before_join,
		added,
		removed,
	} = self
		.services
		.state_compressor
		.save_state(room_id, Arc::new(compressed))
		.await?;

	debug!(
		state_hash = ?statehash_before_join,
		"Forcing state for new room..."
	);
	self.services
		.state
		.force_state(room_id, statehash_before_join, added, removed, state_lock)
		.await?;

	self.services
		.state_cache
		.update_joined_count(room_id)
		.await;

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
	state_lock: RoomMutexGuard,
	extra_content: Option<CanonicalJsonObject>,
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

	let is_joined_restricted_rooms = restriction_rooms
		.iter()
		.stream()
		.any(|restriction_room_id| {
			self.services
				.state_cache
				.is_joined(sender_user, restriction_room_id)
		})
		.await;

	let join_authorized_via_users_server = is_joined_restricted_rooms
		.then_async(async || {
			self.services
				.state_cache
				.local_users_in_room(room_id)
				.filter(|user| {
					self.services.state_accessor.user_can_invite(
						room_id,
						user,
						sender_user,
						&state_lock,
					)
				})
				.map(ToOwned::to_owned)
				.boxed()
				.next()
				.await
		})
		.map(Option::flatten)
		.await;

	let mut content = RoomMemberEventContent {
		reason: reason.clone(),
		join_authorized_via_users_server,
		..RoomMemberEventContent::new(MembershipState::Join)
	};

	self.services
		.profile
		.fill_profile_data(sender_user, &mut content)
		.await;

	let content = merge_member_content(content, extra_content.as_ref())?;

	let pdu_builder = PduBuilder {
		event_type: StateEventType::RoomMember.into(),
		content: to_raw_value(&content).map(Into::into)?,
		state_key: Some(sender_user.to_string().into()),
		..Default::default()
	};

	// Try normal join first
	let Err(error) = self
		.services
		.timeline
		.build_and_append_pdu(pdu_builder, sender_user, room_id, &state_lock)
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

	// Drop before the federation fallback: handle_incoming_pdu re-acquires
	// the same per-room state mutex while ingesting prev_events; deadlock.
	drop(state_lock);

	let Ok((make_join_response, remote_server)) = self
		.make_join_request(sender_user, room_id, servers)
		.await
	else {
		return Err(error);
	};

	let room_version_id = self.require_supported_remote_room_version(&make_join_response)?;
	let room_version_rules = room_version::rules(&room_version_id)?;
	let (join_event, event_id, _) = self
		.create_join_event(
			room_id,
			sender_user,
			&make_join_response.event,
			&room_version_id,
			&room_version_rules,
			reason,
			extra_content,
		)
		.await?;

	let send_join_response = self
		.execute_send_join(&remote_server, room_id, &event_id, join_event, &room_version_id)
		.await?;

	let Some(signed_raw) = send_join_response.event else {
		return Err(error);
	};

	let (signed_event_id, signed_value) =
		gen_event_id_canonical_json(&signed_raw, &room_version_id).map_err(|e| {
			err!(Request(BadJson(warn!("Could not convert event to canonical JSON: {e}"))))
		})?;

	if signed_event_id != event_id {
		return Err!(Request(BadJson(warn!(
			%signed_event_id, %event_id, "Server {remote_server} sent event with wrong event ID"
		))));
	}

	self.services
		.event_handler
		.handle_incoming_pdu(&remote_server, room_id, &signed_event_id, signed_value, true)
		.boxed()
		.await?;

	Ok(())
}

#[implement(Service)]
#[expect(clippy::too_many_arguments)]
#[tracing::instrument(name = "make_join", level = "debug", skip_all)]
async fn create_join_event(
	&self,
	room_id: &RoomId,
	sender_user: &UserId,
	join_event_stub: &RawJsonValue,
	room_version_id: &RoomVersionId,
	room_version_rules: &RoomVersionRules,
	reason: Option<String>,
	extra_content: Option<CanonicalJsonObject>,
) -> Result<(CanonicalJsonObject, OwnedEventId, Option<OwnedUserId>)> {
	let mut event: CanonicalJsonObject =
		serde_json::from_str(join_event_stub.get()).map_err(|e| {
			err!(BadServerResponse("Invalid make_join event json received from server: {e:?}"))
		})?;

	let join_authorized_via_users_server = room_version_rules
		.authorization
		.restricted_join_rule
		.then(|| event.get("content"))
		.flatten()
		.and_then(|s| {
			s.as_object()?
				.get("join_authorised_via_users_server")
		})
		.and_then(|s| OwnedUserId::try_from(s.as_str().unwrap_or_default()).ok());

	let mut content = RoomMemberEventContent {
		reason,
		join_authorized_via_users_server: join_authorized_via_users_server.clone(),
		..RoomMemberEventContent::new(MembershipState::Join)
	};

	self.services
		.profile
		.fill_profile_data(sender_user, &mut content)
		.await;

	let content = merge_member_content(content, extra_content.as_ref())?;

	event.insert("content".into(), content);

	event.insert(
		"origin".into(),
		CanonicalJsonValue::String(
			self.services
				.globals
				.server_name()
				.as_str()
				.to_owned(),
		),
	);

	event.insert(
		"origin_server_ts".into(),
		CanonicalJsonValue::Integer(utils::millis_since_unix_epoch().try_into()?),
	);

	event.insert("room_id".into(), CanonicalJsonValue::String(room_id.as_str().into()));

	event.insert("sender".into(), CanonicalJsonValue::String(sender_user.as_str().into()));

	event.insert("state_key".into(), CanonicalJsonValue::String(sender_user.as_str().into()));

	event.insert("type".into(), CanonicalJsonValue::String("m.room.member".into()));

	let event_id = self
		.services
		.server_keys
		.gen_id_hash_and_sign_event(&mut event, room_version_id)?;

	check_rules(&event, &room_version_rules.event_format)?;

	Ok((event, event_id, join_authorized_via_users_server))
}

// Server-computed membership fields win; client custom keys only fill the gaps.
fn merge_member_content(
	content: RoomMemberEventContent,
	extra_content: Option<&CanonicalJsonObject>,
) -> Result<CanonicalJsonValue> {
	let mut content = to_canonical_value(content)?;

	if let (CanonicalJsonValue::Object(content), Some(extra_content)) =
		(&mut content, extra_content)
	{
		for (key, value) in extra_content {
			content
				.entry(key.clone())
				.or_insert_with(|| value.clone());
		}
	}

	Ok(content)
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
					.config
					.supported_room_versions()
					.map(at!(0))
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

	// Strict via: an explicit remote server in via must not be padded with
	// the room owner, otherwise failover-probe semantics break.
	let has_remote_via = via
		.iter()
		.any(|s| !services.globals.server_is_ours(s));

	if !has_remote_via {
		if let Some(server_name) = room_id.server_name() {
			servers.insert(0, server_name.to_owned());
		}

		if let Some(orig_room_id) = orig_room_id
			&& let Some(orig_server_name) = orig_room_id.server_name()
		{
			servers.insert(0, orig_server_name.to_owned());
		}
	}

	shuffle(&mut additional_servers);

	servers.extend_from_slice(&additional_servers);

	// 1. (room alias server)?
	// 2. (room id server)?
	// 3. shuffle [via query + resolve servers]?
	// 4. shuffle [invited via, inviters servers]?
	debug!(?servers);

	// dedup preserving order
	let mut set = HashSet::new();
	servers.retain(|x| set.insert(x.clone()));
	debug!(?servers);

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

	debug_info!(?servers);
	Ok(servers)
}
