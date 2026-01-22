#![expect(deprecated)]

use std::borrow::Borrow;

use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::try_join4};
use ruma::{
	OwnedEventId, OwnedRoomId, OwnedServerName, OwnedUserId, RoomId, ServerName,
	api::federation::membership::create_join_event,
	events::{
		StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{
	Err, Result, at, err,
	matrix::event::gen_event_id_canonical_json,
	utils::stream::{IterStream, TryBroadbandExt},
	warn,
};
use tuwunel_service::Services;

use crate::Ruma;

/// helper method for /send_join v1 and v2
async fn create_join_event(
	services: &Services,
	origin: &ServerName,
	room_id: &RoomId,
	pdu: &RawJsonValue,
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

	let state_key: OwnedUserId = serde_json::from_value(
		value
			.get("state_key")
			.ok_or_else(|| err!(Request(BadJson("Event missing state_key property."))))?
			.clone()
			.into(),
	)
	.map_err(|e| err!(Request(BadJson(warn!("State key is not a valid user ID: {e}")))))?;

	if state_key != sender {
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

		if !super::user_can_perform_restricted_join(services, &state_key, room_id, &room_version)
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

	// Prestart state gather here since it doesn't involve the new join event.
	let state_ids = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.map(at!(1))
		.collect::<Vec<OwnedEventId>>()
		.boxed();

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

	// Join event for new server.
	let event = services
		.federation
		.format_pdu_into(value, Some(&room_version))
		.map(Some)
		.map(Ok);

	// Join event revealed to existing servers.
	let broadcast = services.sending.send_pdu_room(room_id, &pdu_id);

	// Wait for state gather which the remaining operations depend on.
	let state_ids = state_ids.await;
	let auth_heads = state_ids.iter().map(Borrow::borrow);
	let into_federation_format = |pdu| {
		services
			.federation
			.format_pdu_into(pdu, Some(&room_version))
			.map(Ok)
	};

	let auth_chain = services
		.auth_chain
		.event_ids_iter(room_id, &room_version, auth_heads)
		.broad_and_then(async |event_id| {
			services
				.timeline
				.get_pdu_json(&event_id)
				.and_then(into_federation_format)
				.await
		})
		.try_collect();

	let state = state_ids
		.iter()
		.try_stream()
		.broad_and_then(|event_id| {
			services
				.timeline
				.get_pdu_json(event_id)
				.and_then(into_federation_format)
		})
		.try_collect();

	let (auth_chain, state, event, ()) = try_join4(auth_chain, state, event, broadcast)
		.boxed()
		.await?;

	Ok(create_join_event::v1::RoomState { auth_chain, state, event })
}

/// # `PUT /_matrix/federation/v1/send_join/{roomId}/{eventId}`
///
/// Submits a signed join event.
pub(crate) async fn create_join_event_v1_route(
	State(services): State<crate::State>,
	body: Ruma<create_join_event::v1::Request>,
) -> Result<create_join_event::v1::Response> {
	if let Some(server) = body.room_id.server_name()
		&& services
			.config
			.forbidden_remote_server_names
			.is_match(server.host())
	{
		warn!(
			"Server {} tried joining room ID {} through us which has a server name that is \
			 globally forbidden. Rejecting.",
			body.origin(),
			&body.room_id,
		);

		return Err!(Request(Forbidden(warn!(
			"Room ID server name {server} is banned on this homeserver."
		))));
	}

	Ok(create_join_event::v1::Response {
		room_state: create_join_event(&services, body.origin(), &body.room_id, &body.pdu)
			.boxed()
			.await?,
	})
}

/// # `PUT /_matrix/federation/v2/send_join/{roomId}/{eventId}`
///
/// Submits a signed join event.
pub(crate) async fn create_join_event_v2_route(
	State(services): State<crate::State>,
	body: Ruma<create_join_event::v2::Request>,
) -> Result<create_join_event::v2::Response> {
	if let Some(server) = body.room_id.server_name()
		&& services
			.config
			.forbidden_remote_server_names
			.is_match(server.host())
	{
		warn!(
			"Server {} tried joining room ID {} through us which has a server name that is \
			 globally forbidden. Rejecting.",
			body.origin(),
			&body.room_id,
		);

		return Err!(Request(Forbidden(warn!(
			"Room ID server name {server} is banned on this homeserver."
		))));
	}

	let create_join_event::v1::RoomState { auth_chain, state, event } =
		create_join_event(&services, body.origin(), &body.room_id, &body.pdu)
			.boxed()
			.await?;

	Ok(create_join_event::v2::Response {
		room_state: create_join_event::v2::RoomState {
			members_omitted: false,
			auth_chain,
			state,
			event,
			servers_in_room: None,
		},
	})
}
