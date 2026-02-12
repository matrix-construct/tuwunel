use axum::extract::State;
use futures::{FutureExt, TryFutureExt, TryStreamExt};
use ruma::{
	OwnedEventId, RoomId, UserId,
	api::client::state::{get_state_event_for_key, get_state_events, send_state_event},
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
	utils::BoolExt,
};
use tuwunel_service::Services;

use crate::{Ruma, RumaResponse};

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

	Ok(get_state_events::v3::Response {
		room_state: services
			.state_accessor
			.room_state_full_pdus(&body.room_id)
			.map_ok(Event::into_format)
			.try_collect()
			.await?,
	})
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
		.map_err(|_| {
			err!(Request(NotFound(debug_warn!(
					room_id = ?body.room_id,
					event_type = ?body.event_type,
					"State event not found in room.",
			))))
		})?;

	let event_format = body
		.format
		.as_deref()
		.is_some_and(|f| f.eq_ignore_ascii_case("event"));

	Ok(get_state_event_for_key::v3::Response {
		content: event_format.or(|| event.get_content_as_value()),
		event: event_format.then(|| {
			json!({
				"content": event.content(),
				"event_id": event.event_id(),
				"origin_server_ts": event.origin_server_ts(),
				"room_id": event.room_id(),
				"sender": event.sender(),
				"state_key": event.state_key(),
				"type": event.kind(),
				"unsigned": event.unsigned(),
			})
		}),
	})
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
	allowed_to_send_state_event(services, room_id, event_type, state_key, json).await?;
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
	room_id: &RoomId,
	event_type: &StateEventType,
	state_key: &str,
	json: &Raw<AnyStateEventContent>,
) -> Result {
	match event_type {
		| StateEventType::RoomCreate => {
			return Err!(Request(BadJson(debug_warn!(
				?room_id,
				"You cannot update m.room.create after a room has been created."
			))));
		},
		| StateEventType::RoomServerAcl => {
			// prevents common ACL paw-guns as ACL management is difficult and prone to
			// irreversible mistakes
			match json.deserialize_as_unchecked::<RoomServerAclEventContent>() {
				| Ok(acl_content) => {
					if acl_content.allow_is_empty() {
						return Err!(Request(BadJson(debug_warn!(
							?room_id,
							"Sending an ACL event with an empty allow key will permanently \
							 brick the room for non-tuwunel's as this equates to no servers \
							 being allowed to participate in this room."
						))));
					}

					if acl_content.deny_contains("*") && acl_content.allow_contains("*") {
						return Err!(Request(BadJson(debug_warn!(
							?room_id,
							"Sending an ACL event with a deny and allow key value of \"*\" will \
							 permanently brick the room for non-tuwunel's as this equates to no \
							 servers being allowed to participate in this room."
						))));
					}

					if acl_content.deny_contains("*")
						&& !acl_content.is_allowed(services.globals.server_name())
						&& !acl_content.allow_contains(services.globals.server_name().as_str())
					{
						return Err!(Request(BadJson(debug_warn!(
							?room_id,
							"Sending an ACL event with a deny key value of \"*\" and without \
							 your own server name in the allow key will result in you being \
							 unable to participate in this room."
						))));
					}

					if !acl_content.allow_contains("*")
						&& !acl_content.is_allowed(services.globals.server_name())
						&& !acl_content.allow_contains(services.globals.server_name().as_str())
					{
						return Err!(Request(BadJson(debug_warn!(
							?room_id,
							"Sending an ACL event for an allow key without \"*\" and without \
							 your own server name in the allow key will result in you being \
							 unable to participate in this room."
						))));
					}
				},
				| Err(e) => {
					return Err!(Request(BadJson(debug_warn!(
						"Room server ACL event is invalid: {e}"
					))));
				},
			}
		},
		| StateEventType::RoomEncryption =>
		// Forbid m.room.encryption if encryption is disabled
			if !services.config.allow_encryption {
				return Err!(Request(Forbidden("Encryption is disabled on this homeserver.")));
			},
		| StateEventType::RoomJoinRules => {
			// admin room is a sensitive room, it should not ever be made public
			if let Ok(admin_room_id) = services.admin.get_admin_room().await
				&& admin_room_id == room_id
			{
				match json.deserialize_as_unchecked::<RoomJoinRulesEventContent>() {
					| Ok(join_rule) =>
						if join_rule.join_rule == JoinRule::Public {
							return Err!(Request(Forbidden(
								"Admin room is a sensitive room, it cannot be made public"
							)));
						},
					| Err(e) => {
						return Err!(Request(BadJson(debug_warn!(
							"Room join rules event is invalid: {e}"
						))));
					},
				}
			}
		},
		| StateEventType::RoomHistoryVisibility => {
			// admin room is a sensitive room, it should not ever be made world readable
			if let Ok(admin_room_id) = services.admin.get_admin_room().await {
				match json.deserialize_as_unchecked::<RoomHistoryVisibilityEventContent>() {
					| Ok(visibility_content) => {
						if admin_room_id == room_id
							&& visibility_content.history_visibility
								== HistoryVisibility::WorldReadable
						{
							return Err!(Request(Forbidden(
								"Admin room is a sensitive room, it cannot be made world \
								 readable (public room history)."
							)));
						}
					},
					| Err(e) => {
						return Err!(Request(BadJson(debug_warn!(
							"Room history visibility event is invalid: {e}"
						))));
					},
				}
			}
		},
		| StateEventType::RoomCanonicalAlias => {
			match json.deserialize_as_unchecked::<RoomCanonicalAliasEventContent>() {
				| Ok(canonical_alias_content) => {
					let mut aliases = canonical_alias_content.alt_aliases.clone();

					if let Some(alias) = canonical_alias_content.alias {
						aliases.push(alias);
					}

					for alias in aliases {
						let (alias_room_id, _servers) = services
							.alias
							.resolve_alias(&alias)
							.await
							.map_err(|e| {
								err!(Request(BadAlias("Failed resolving alias \"{alias}\": {e}")))
							})?;

						if alias_room_id != room_id {
							return Err!(Request(BadAlias(
								"Room alias {alias} does not belong to room {room_id}"
							)));
						}
					}
				},
				| Err(e) => {
					return Err!(Request(InvalidParam(debug_warn!(
						"Room canonical alias event is invalid: {e}"
					))));
				},
			}
		},
		| StateEventType::RoomMember => {
			match json.deserialize_as_unchecked::<RoomMemberEventContent>() {
				| Ok(membership_content) => {
					let Ok(_state_key) = UserId::parse(state_key) else {
						return Err!(Request(BadJson(
							"Membership event has invalid or non-existent state key"
						)));
					};

					if let Some(authorising_user) =
						membership_content.join_authorized_via_users_server
					{
						if membership_content.membership != MembershipState::Join {
							return Err!(Request(BadJson(
								"join_authorised_via_users_server is only for member joins"
							)));
						}

						if !services.globals.user_is_local(&authorising_user) {
							return Err!(Request(InvalidParam(
								"Authorising user {authorising_user} does not belong to this \
								 homeserver"
							)));
						}

						services
							.state_cache
							.is_joined(&authorising_user, room_id)
							.map(is_false!())
							.map(BoolExt::into_result)
							.map_err(|()| {
								err!(Request(InvalidParam(
									"Authorising user {authorising_user} is not in the room. \
									 They cannot authorise the join."
								)))
							})
							.await?;
					}
				},
				| Err(e) => {
					return Err!(Request(BadJson(
						"Membership content must have a valid JSON body with at least a valid \
						 membership state: {e}"
					)));
				},
			}
		},
		| _ => (),
	}

	Ok(())
}
