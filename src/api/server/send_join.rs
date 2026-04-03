use std::borrow::Borrow;

use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::try_join4};
use ruma::{
	CanonicalJsonObject, OwnedEventId, OwnedRoomId, OwnedServerName, OwnedUserId, RoomId,
	ServerName,
	api::federation::membership::create_join_event,
	events::{
		StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{
	Err, Result, at, debug_error, err,
	itertools::Itertools,
	matrix::event::gen_event_id_canonical_json,
	utils::{
		BoolExt,
		future::{BoolExt as _, ReadyBoolExt},
		stream::{BroadbandExt, IterStream, TryBroadbandExt, TryReadyExt},
	},
	warn,
};
use tuwunel_service::Services;

use crate::{Ruma, client::sync::calculate_heroes};

/// # `PUT /_matrix/federation/v2/send_join/{roomId}/{eventId}`
///
/// Submits a signed join event.
pub(crate) async fn create_join_event_v2_route(
	State(services): State<crate::State>,
	body: Ruma<create_join_event::v2::Request>,
) -> Result<create_join_event::v2::Response> {
	let room_id = &body.room_id;
	let origin = body.origin();
	let members_omitted = body.omit_members;

	if let Some(server) = room_id.server_name()
		&& services
			.config
			.is_forbidden_remote_server_name(server)
	{
		warn!(
			"Server {origin} tried joining {room_id} through us which has a server name that is \
			 globally forbidden. Rejecting.",
		);

		return Err!(Request(Forbidden(warn!(
			"Room ID server name {server} is banned on this homeserver."
		))));
	}

	// Get the servers in the room BEFORE the join
	let servers_in_room = members_omitted
		.then_async(|| {
			services
				.state_cache
				.room_servers(room_id)
				.map(ToOwned::to_owned)
				.collect::<Vec<_>>()
		})
		.await;

	let create_join_event::v1::RoomState { auth_chain, state, event } =
		create_join_event(&services, origin, room_id, &body.pdu, members_omitted)
			.boxed()
			.await?;

	Ok(create_join_event::v2::Response {
		room_state: create_join_event::v2::RoomState {
			auth_chain,
			state,
			event,
			servers_in_room,
			members_omitted,
		},
	})
}

async fn create_join_event(
	services: &Services,
	origin: &ServerName,
	room_id: &RoomId,
	pdu: &RawJsonValue,
	omit_members: bool,
) -> Result<create_join_event::v1::RoomState> {
	if !services.metadata.exists(room_id).await {
		return Err!(Request(NotFound("Room is unknown to this server.")));
	}

	// ACL check origin server
	services
		.event_handler
		.acl_check(origin, room_id)
		.await?;

	// We need to return the state prior to joining, let's keep a reference to that
	// here
	let shortstatehash = services
		.state
		.get_room_shortstatehash(room_id)
		.await
		.map_err(|e| err!(Request(NotFound(error!("Room has no state: {e}")))))?;

	// We do not add the event_id field to the pdu here because of signature and
	// hashes checks
	let room_version = services.state.get_room_version(room_id).await?;

	let Ok((event_id, mut value)) = gen_event_id_canonical_json(pdu, &room_version) else {
		// Event could not be converted to canonical json
		return Err!(Request(BadJson("Could not convert event to canonical json.")));
	};

	let event_room_id: OwnedRoomId = serde_json::from_value(
		value
			.get("room_id")
			.ok_or_else(|| err!(Request(BadJson("Event missing room_id property."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson(warn!("room_id field is not a valid room ID: {e}")))))?;

	if event_room_id != room_id {
		return Err!(Request(BadJson("Event room_id does not match request path room ID.")));
	}

	let event_type: StateEventType = serde_json::from_value(
		value
			.get("type")
			.ok_or_else(|| err!(Request(BadJson("Event missing type property."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson(warn!("Event has invalid state event type: {e}")))))?;

	if event_type != StateEventType::RoomMember {
		return Err!(Request(BadJson(
			"Not allowed to send non-membership state event to join endpoint."
		)));
	}

	let content: RoomMemberEventContent = serde_json::from_value(
		value
			.get("content")
			.ok_or_else(|| err!(Request(BadJson("Event missing content property"))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson(warn!("Event content is empty or invalid: {e}")))))?;

	if content.membership != MembershipState::Join {
		return Err!(Request(BadJson(
			"Not allowed to send a non-join membership event to join endpoint."
		)));
	}

	// ACL check sender user server name
	let sender: OwnedUserId = serde_json::from_value(
		value
			.get("sender")
			.ok_or_else(|| err!(Request(BadJson("Event missing sender property."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson(warn!("sender property is not a valid user ID: {e}")))))?;

	services
		.event_handler
		.acl_check(sender.server_name(), room_id)
		.await?;

	// check if origin server is trying to send for another server
	if sender.server_name() != origin {
		return Err!(Request(Forbidden("Not allowed to join on behalf of another server.")));
	}

	let joining_user: OwnedUserId = serde_json::from_value(
		value
			.get("state_key")
			.ok_or_else(|| err!(Request(BadJson("Event missing state_key property."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson(warn!("State key is not a valid user ID: {e}")))))?;

	if joining_user != sender {
		return Err!(Request(BadJson("State key does not match sender user.")));
	}

	if let Some(authorising_user) = content.join_authorized_via_users_server {
		use ruma::RoomVersionId::*;

		if matches!(room_version, V1 | V2 | V3 | V4 | V5 | V6 | V7) {
			return Err!(Request(InvalidParam(
				"Room version {room_version} does not support restricted rooms but \
				 join_authorised_via_users_server ({authorising_user}) was found in the event."
			)));
		}

		if !services.globals.user_is_local(&authorising_user) {
			return Err!(Request(InvalidParam(
				"Cannot authorise membership event through {authorising_user} as they do not \
				 belong to this homeserver"
			)));
		}

		if !services
			.state_cache
			.is_joined(&authorising_user, room_id)
			.await
		{
			return Err!(Request(InvalidParam(
				"Authorising user {authorising_user} is not in the room you are trying to join, \
				 they cannot authorise your join."
			)));
		}

		if !super::user_can_perform_restricted_join(
			services,
			&joining_user,
			room_id,
			&room_version,
		)
		.await?
		{
			return Err!(Request(UnableToAuthorizeJoin(
				"Joining user did not pass restricted room's rules."
			)));
		}
	}

	services
		.server_keys
		.hash_and_sign_event(&mut value, &room_version)
		.map_err(|e| err!(Request(InvalidParam(warn!("Failed to sign send_join event: {e}")))))?;

	let origin: OwnedServerName = serde_json::from_value(
		value
			.get("origin")
			.ok_or_else(|| err!(Request(BadJson("Event does not have an origin server name."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson("Event has an invalid origin server name: {e}"))))?;

	// MSC3943: Only include heroes when the room has no name and no
	// canonical alias (matching Synapse's behavior in PR #14442).
	let heroes = omit_members
		.then_async(|| {
			let has_name = services.state_accessor.state_contains(
				shortstatehash,
				&StateEventType::RoomName,
				"",
			);

			let has_alias = services.state_accessor.state_contains(
				shortstatehash,
				&StateEventType::RoomCanonicalAlias,
				"",
			);

			has_name
				.is_false()
				.and(has_alias.is_false())
				.then(|_| calculate_heroes(services, room_id, &joining_user))
		})
		.await
		.unwrap_or_default();

	// Prestart state gather here since it doesn't involve the new join event.
	let state_ids = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.broad_filter_map(async |(ssk, event_id)| {
			// Filter state: keep all non-member events, the joining user's
			// member event, and hero member events. If get_statekey_from_short
			// fails, keep the event (safe default, matching original behavior).
			if omit_members
				&& let Ok((kind, sk)) = services.short.get_statekey_from_short(ssk).await
				&& kind == StateEventType::RoomMember
				&& let Ok(user_id) = sk.as_str().try_into()
				&& joining_user != user_id
				&& !heroes.contains(&user_id)
			{
				return None;
			}

			Some(event_id)
		})
		.collect::<Vec<_>>();

	let mutex_lock = services
		.event_handler
		.mutex_federation
		.lock(room_id)
		.await;

	let pdu_id = services
		.event_handler
		.handle_incoming_pdu(&origin, room_id, &event_id, value.clone(), true)
		.boxed()
		.await?
		.map(at!(0))
		.ok_or_else(|| err!(Request(InvalidParam("Could not accept as timeline event."))))?;

	drop(mutex_lock);

	// Wait for state gather which the remaining operations depend on.
	let state_ids = state_ids
		.await
		.into_iter()
		.sorted_unstable()
		.collect::<Vec<_>>();

	let into_federation_format = |pdu: CanonicalJsonObject| {
		services
			.federation
			.format_pdu_into(pdu, Some(&room_version))
			.map(Ok)
	};

	// MSC3706: Any events returned within state can be omitted from auth_chain.
	let include_auth_event =
		|event_id: &OwnedEventId| !omit_members || state_ids.binary_search(event_id).is_err();

	let auth_heads = state_ids.iter().map(Borrow::borrow);

	let auth_chain = services
		.auth_chain
		.event_ids_iter(room_id, &room_version, auth_heads)
		.ready_try_filter(include_auth_event)
		.broad_and_then(async |event_id| {
			services
				.timeline
				.get_pdu_json(&event_id)
				.and_then(into_federation_format)
				.inspect_err(|e| debug_error!(?event_id, "auth_chain event not found: {e}"))
				.await
		})
		.try_collect();

	let state = state_ids
		.iter()
		.try_stream()
		.broad_and_then(async |event_id| {
			services
				.timeline
				.get_pdu_json(event_id)
				.and_then(into_federation_format)
				.inspect_err(|e| debug_error!(?event_id, "state event not found: {e}"))
				.await
		})
		.try_collect();

	// Join event for new server.
	let event = services
		.federation
		.format_pdu_into(value, Some(&room_version))
		.map(Some)
		.map(Ok);

	// Join event revealed to existing servers.
	let broadcast = services.sending.send_pdu_room(room_id, &pdu_id);

	let (auth_chain, state, event, ()) = try_join4(auth_chain, state, event, broadcast)
		.boxed()
		.await?;

	Ok(create_join_event::v1::RoomState { auth_chain, state, event })
}
