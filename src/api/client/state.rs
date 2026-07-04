use axum::extract::State;
use futures::{FutureExt, TryFutureExt, TryStreamExt};
use ruma::{
	OwnedEventId, OwnedRoomAliasId, RoomId, UserId,
	api::client::state::{
		get_state_event_for_key::{self, v3::StateEventFormat},
		get_state_events, send_state_event,
	},
	events::{
		AnyStateEventContent, StateEventType,
		room::{
			canonical_alias::RoomCanonicalAliasEventContent,
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
			server_acl::RoomServerAclEventContent,
		},
	},
	serde::Raw,
};
use serde_json::json;
use tuwunel_core::{
	Err, Result, err, is_false,
	matrix::{Event, pdu::PduBuilder},
	utils::{BoolExt, stream::TryBroadbandExt},
};
use tuwunel_service::Services;

use crate::{Ruma, RumaResponse, client::with_membership};

/// # `PUT /_matrix/client/*/rooms/{roomId}/state/{eventType}/{stateKey}`
///
/// Sends a state event into the room.
pub(crate) async fn send_state_event_for_key_route(
	State(services): State<crate::State>,
	body: Ruma<send_state_event::v3::Request>,
) -> Result<send_state_event::v3::Response> {
	let sender_user = body.sender_user();

	Ok(send_state_event::v3::Response {
		event_id: send_state_event_for_key_helper(
			&services,
			sender_user,
			&body.room_id,
			&body.event_type,
			&body.body.body,
			&body.state_key,
			if body.appservice_info.is_some() {
				body.timestamp
			} else {
				None
			},
		)
		.await?,
	})
}

/// # `PUT /_matrix/client/*/rooms/{roomId}/state/{eventType}`
///
/// Sends a state event into the room.
pub(crate) async fn send_state_event_for_empty_key_route(
	State(services): State<crate::State>,
	body: Ruma<send_state_event::v3::Request>,
) -> Result<RumaResponse<send_state_event::v3::Response>> {
	send_state_event_for_key_route(State(services), body)
		.boxed()
		.await
		.map(RumaResponse)
}

/// # `GET /_matrix/client/v3/rooms/{roomid}/state`
///
/// Get all state events for a room.
///
/// - If not joined: Only works if current room history visibility is world
///   readable
pub(crate) async fn get_state_events_route(
	State(services): State<crate::State>,
	body: Ruma<get_state_events::v3::Request>,
) -> Result<get_state_events::v3::Response> {
	let sender_user = body.sender_user();

	if !services
		.state_accessor
		.user_can_see_state_events(sender_user, &body.room_id)
		.await
	{
		return Err!(Request(Forbidden("You don't have permission to view the room state.")));
	}

	let encrypted = services
		.state_accessor
		.is_encrypted_room(&body.room_id)
		.await;

	let room_state = services
		.state_accessor
		.room_state_full_pdus(&body.room_id)
		.map_ok(Event::into_pdu)
		.broad_and_then(async |pdu| {
			Ok(with_membership(&services, pdu, sender_user, encrypted).await)
		})
		.map_ok(Event::into_format)
		.try_collect()
		.await?;

	Ok(get_state_events::v3::Response { room_state })
}

/// # `GET /_matrix/client/v3/rooms/{roomid}/state/{eventType}/{stateKey}`
///
/// Get single state event of a room with the specified state key.
/// The optional query parameter `?format=event|content` allows returning the
/// full room state event or just the state event's content (default behaviour)
///
/// - If not joined: Only works if current room history visibility is world
///   readable
pub(crate) async fn get_state_events_for_key_route(
	State(services): State<crate::State>,
	body: Ruma<get_state_event_for_key::v3::Request>,
) -> Result<get_state_event_for_key::v3::Response> {
	let sender_user = body.sender_user();

	if !services
		.state_accessor
		.user_can_see_state_events(sender_user, &body.room_id)
		.await
	{
		return Err!(Request(NotFound(debug_warn!(
			"You don't have permission to view the room state."
		))));
	}

	let event = services
		.state_accessor
		.room_state_get(&body.room_id, &body.event_type, &body.state_key)
		.await
		.map_err(|e| {
			err!(Request(NotFound(debug_warn!(
				room_id = ?body.room_id,
				event_type = ?body.event_type,
				"Failed to get state event: {e}.",
			))))
		})?;

	let event_or_content = match body.format {
		| StateEventFormat::Event => json!({
			"content": event.content(),
			"event_id": event.event_id(),
			"origin_server_ts": event.origin_server_ts(),
			"room_id": event.room_id(),
			"sender": event.sender(),
			"state_key": event.state_key(),
			"type": event.kind(),
			"unsigned": event.unsigned(),
		}),

		| _ => event.get_content_as_value(),
	};

	let event_or_content =
		serde_json::value::to_raw_value(&event_or_content).expect("serializable JSON value");

	Ok(get_state_event_for_key::v3::Response::new(event_or_content))
}

/// # `GET /_matrix/client/v3/rooms/{roomid}/state/{eventType}`
///
/// Get single state event of a room.
/// The optional query parameter `?format=event|content` allows returning the
/// full room state event or just the state event's content (default behaviour)
///
/// - If not joined: Only works if current room history visibility is world
///   readable
pub(crate) async fn get_state_events_for_empty_key_route(
	State(services): State<crate::State>,
	body: Ruma<get_state_event_for_key::v3::Request>,
) -> Result<RumaResponse<get_state_event_for_key::v3::Response>> {
	get_state_events_for_key_route(State(services), body)
		.await
		.map(RumaResponse)
}

async fn send_state_event_for_key_helper(
	services: &Services,
	sender: &UserId,
	room_id: &RoomId,
	event_type: &StateEventType,
	json: &Raw<AnyStateEventContent>,
	state_key: &str,
	timestamp: Option<ruma::MilliSecondsSinceUnixEpoch>,
) -> Result<OwnedEventId> {
	allowed_to_send_state_event(services, sender, room_id, event_type, state_key, json).await?;
	let state_lock = services.state.mutex.lock(room_id).await;
	let event_id = services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: event_type.to_string().into(),
				content: serde_json::from_str(json.json().get())?,
				state_key: Some(state_key.into()),
				timestamp,
				..Default::default()
			},
			sender,
			room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	Ok(event_id)
}

async fn allowed_to_send_state_event(
	services: &Services,
	sender: &UserId,
	room_id: &RoomId,
	event_type: &StateEventType,
	state_key: &str,
	json: &Raw<AnyStateEventContent>,
) -> Result {
	let suspended = services.users.is_suspended(sender).await;

	if suspended && !matches!(event_type, StateEventType::RoomMember) {
		return Err!(Request(UserSuspended("Account is suspended.")));
	}

	match event_type {
		| StateEventType::RoomCreate => Err!(Request(BadJson(debug_warn!(
			?room_id,
			"You cannot update m.room.create after a room has been created."
		)))),
		| StateEventType::RoomServerAcl => validate_server_acl(services, room_id, json),
		| StateEventType::RoomEncryption => validate_encryption(services),
		| StateEventType::RoomJoinRules => validate_join_rules(services, room_id, json).await,
		| StateEventType::RoomHistoryVisibility =>
			validate_history_visibility(services, room_id, json).await,
		| StateEventType::RoomCanonicalAlias =>
			validate_canonical_alias(services, room_id, json).await,
		| StateEventType::RoomMember =>
			validate_member(services, sender, room_id, state_key, json, suspended).await,
		| _ => Ok(()),
	}
}

fn validate_encryption(services: &Services) -> Result {
	services
		.config
		.allow_encryption
		.then_some(())
		.ok_or_else(|| err!(Request(Forbidden("Encryption is disabled on this homeserver."))))
}

fn validate_server_acl(
	services: &Services,
	room_id: &RoomId,
	json: &Raw<AnyStateEventContent>,
) -> Result {
	let acl_content = json
		.deserialize_as_unchecked::<RoomServerAclEventContent>()
		.map_err(|e| {
			err!(Request(BadJson(debug_warn!("Room server ACL event is invalid: {e}"))))
		})?;

	if acl_content.allow_is_empty() {
		return Err!(Request(BadJson(debug_warn!(
			?room_id,
			"Sending an ACL event with an empty allow key will permanently brick the room for \
			 non-tuwunel's as this equates to no servers being allowed to participate in this \
			 room."
		))));
	}

	if acl_content.deny_contains("*") && acl_content.allow_contains("*") {
		return Err!(Request(BadJson(debug_warn!(
			?room_id,
			"Sending an ACL event with a deny and allow key value of \"*\" will permanently \
			 brick the room for non-tuwunel's as this equates to no servers being allowed to \
			 participate in this room."
		))));
	}

	let server_name = services.globals.server_name();
	let self_allowed =
		acl_content.is_allowed(server_name) || acl_content.allow_contains(server_name.as_str());

	if acl_content.deny_contains("*") && !self_allowed {
		return Err!(Request(BadJson(debug_warn!(
			?room_id,
			"Sending an ACL event with a deny key value of \"*\" and without your own server \
			 name in the allow key will result in you being unable to participate in this room."
		))));
	}

	if !acl_content.allow_contains("*") && !self_allowed {
		return Err!(Request(BadJson(debug_warn!(
			?room_id,
			"Sending an ACL event for an allow key without \"*\" and without your own server \
			 name in the allow key will result in you being unable to participate in this room."
		))));
	}

	Ok(())
}

async fn validate_join_rules(
	services: &Services,
	room_id: &RoomId,
	json: &Raw<AnyStateEventContent>,
) -> Result {
	let Ok(admin_room_id) = services.admin.get_admin_room().await else {
		return Ok(());
	};

	if admin_room_id != room_id {
		return Ok(());
	}

	let join_rule = json
		.deserialize_as_unchecked::<RoomJoinRulesEventContent>()
		.map_err(|e| {
			err!(Request(BadJson(debug_warn!("Room join rules event is invalid: {e}"))))
		})?;

	if join_rule.join_rule == JoinRule::Public {
		return Err!(Request(Forbidden(
			"Admin room is a sensitive room, it cannot be made public"
		)));
	}

	Ok(())
}

async fn validate_history_visibility(
	services: &Services,
	room_id: &RoomId,
	json: &Raw<AnyStateEventContent>,
) -> Result {
	let Ok(admin_room_id) = services.admin.get_admin_room().await else {
		return Ok(());
	};

	let visibility_content = json
		.deserialize_as_unchecked::<RoomHistoryVisibilityEventContent>()
		.map_err(|e| {
			err!(Request(BadJson(debug_warn!("Room history visibility event is invalid: {e}"))))
		})?;

	if admin_room_id == room_id
		&& visibility_content.history_visibility == HistoryVisibility::WorldReadable
	{
		return Err!(Request(Forbidden(
			"Admin room is a sensitive room, it cannot be made world readable (public room \
			 history)."
		)));
	}

	Ok(())
}

async fn validate_canonical_alias(
	services: &Services,
	room_id: &RoomId,
	json: &Raw<AnyStateEventContent>,
) -> Result {
	let canonical_alias_content = json
		.deserialize_as_unchecked::<RoomCanonicalAliasEventContent>()
		.map_err(|e| {
			err!(Request(InvalidParam(debug_warn!("Room canonical alias event is invalid: {e}"))))
		})?;

	let current_aliases: Vec<OwnedRoomAliasId> = services
		.state_accessor
		.room_state_get_content::<RoomCanonicalAliasEventContent>(
			room_id,
			&StateEventType::RoomCanonicalAlias,
			"",
		)
		.await
		.ok()
		.map(|content| content.aliases().cloned().collect())
		.unwrap_or_default();

	let new_aliases = canonical_alias_content
		.aliases()
		.filter(|alias| !current_aliases.contains(alias));

	for alias in new_aliases {
		let (alias_room_id, _servers) = services
			.alias
			.resolve_alias(alias)
			.await
			.map_err(|e| err!(Request(BadAlias("Failed resolving alias \"{alias}\": {e}"))))?;

		if alias_room_id != room_id {
			return Err!(Request(BadAlias(
				"Room alias {alias} does not belong to room {room_id}"
			)));
		}
	}

	Ok(())
}

async fn validate_member(
	services: &Services,
	sender: &UserId,
	room_id: &RoomId,
	state_key: &str,
	json: &Raw<AnyStateEventContent>,
	suspended: bool,
) -> Result {
	let membership_content = json
		.deserialize_as_unchecked::<RoomMemberEventContent>()
		.map_err(|e| {
			err!(Request(BadJson(
				"Membership content must have a valid JSON body with at least a valid \
				 membership state: {e}"
			)))
		})?;

	let Ok(target_user) = UserId::parse(state_key) else {
		return Err!(Request(BadJson("Membership event has invalid or non-existent state key")));
	};

	if suspended
		&& (membership_content.membership != MembershipState::Leave || target_user != sender)
	{
		return Err!(Request(UserSuspended("Account is suspended.")));
	}

	if membership_content.membership == MembershipState::Invite
		&& services.globals.user_is_local(&target_user)
		&& services.users.invites_blocked(&target_user).await
	{
		return Err!(Request(InviteBlocked("{target_user} has blocked invites.")));
	}

	let Some(authorising_user) = membership_content.join_authorized_via_users_server else {
		return Ok(());
	};

	if membership_content.membership != MembershipState::Join {
		return Err!(Request(BadJson(
			"join_authorised_via_users_server is only for member joins"
		)));
	}

	// Already joined or invited: no restricted-join authorisation needed.
	if services
		.state_cache
		.user_membership(&target_user, room_id)
		.await
		.is_some_and(|m| matches!(m, MembershipState::Join | MembershipState::Invite))
	{
		return Ok(());
	}

	if !services.globals.user_is_local(&authorising_user) {
		return Err!(Request(InvalidParam(
			"Authorising user {authorising_user} does not belong to this homeserver"
		)));
	}

	services
		.state_cache
		.is_joined(&authorising_user, room_id)
		.map(is_false!())
		.map(BoolExt::into_result)
		.map_err(|()| {
			err!(Request(InvalidParam(
				"Authorising user {authorising_user} is not in the room. They cannot authorise \
				 the join."
			)))
		})
		.await
}
