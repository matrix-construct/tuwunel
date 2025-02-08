use axum::extract::State;
use conduwuit::{err, pdu::PduBuilder, utils::BoolExt, Err, PduEvent, Result};
use futures::TryStreamExt;
use ruma::{
	api::client::state::{get_state_events, get_state_events_for_key, send_state_event},
	events::{
		room::{
			canonical_alias::RoomCanonicalAliasEventContent,
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
		},
		AnyStateEventContent, StateEventType,
	},
	serde::Raw,
	OwnedEventId, RoomId, UserId,
};
use service::Services;

use crate::{Ruma, RumaResponse};

/// # `PUT /_matrix/client/*/rooms/{roomId}/state/{eventType}/{stateKey}`
///
/// Sends a state event into the room.
pub(crate) async fn send_state_event_for_key_route(
	State(services): State<crate::State>,
	body: Ruma<send_state_event::v3::Request>,
) -> Result<send_state_event::v3::Response> {
	let sender_user = body.sender_user.as_ref().expect("user is authenticated");

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
	let sender_user = body.sender_user.as_ref().expect("user is authenticated");

	if !services
		.rooms
		.state_accessor
		.user_can_see_state_events(sender_user, &body.room_id)
		.await
	{
		return Err!(Request(Forbidden("You don't have permission to view the room state.")));
	}

	Ok(get_state_events::v3::Response {
		room_state: services
			.rooms
			.state_accessor
			.room_state_full_pdus(&body.room_id)
			.map_ok(PduEvent::into_state_event)
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
	body: Ruma<get_state_events_for_key::v3::Request>,
) -> Result<get_state_events_for_key::v3::Response> {
	let sender_user = body.sender_user.as_ref().expect("user is authenticated");

	if !services
		.rooms
		.state_accessor
		.user_can_see_state_events(sender_user, &body.room_id)
		.await
	{
		return Err!(Request(Forbidden("You don't have permission to view the room state.")));
	}

	let event = services
		.rooms
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
		.as_ref()
		.is_some_and(|f| f.to_lowercase().eq("event"));

	Ok(get_state_events_for_key::v3::Response {
		content: event_format.or(|| event.get_content_as_value()),
		event: event_format.then(|| event.into_state_event_value()),
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
	body: Ruma<get_state_events_for_key::v3::Request>,
) -> Result<RumaResponse<get_state_events_for_key::v3::Response>> {
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
	let state_lock = services.rooms.state.mutex.lock(room_id).await;
	let event_id = services
		.rooms
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
			return Err!(Request(BadJson(
				"You cannot update m.room.create after a room has been created."
			)));
		},
		// Forbid m.room.encryption if encryption is disabled
		| StateEventType::RoomEncryption =>
			if !services.globals.allow_encryption() {
				return Err!(Request(Forbidden("Encryption is disabled on this homeserver.")));
			},
		// admin room is a sensitive room, it should not ever be made public
		| StateEventType::RoomJoinRules => {
			if let Ok(admin_room_id) = services.admin.get_admin_room().await {
				if admin_room_id == room_id {
					if let Ok(join_rule) =
						serde_json::from_str::<RoomJoinRulesEventContent>(json.json().get())
					{
						if join_rule.join_rule == JoinRule::Public {
							return Err!(Request(Forbidden(
								"Admin room is a sensitive room, it cannot be made public"
							)));
						}
					}
				}
			}
		},
		// admin room is a sensitive room, it should not ever be made world readable
		| StateEventType::RoomHistoryVisibility => {
			if let Ok(visibility_content) =
				serde_json::from_str::<RoomHistoryVisibilityEventContent>(json.json().get())
			{
				if let Ok(admin_room_id) = services.admin.get_admin_room().await {
					if admin_room_id == room_id
						&& visibility_content.history_visibility
							== HistoryVisibility::WorldReadable
					{
						return Err!(Request(Forbidden(
							"Admin room is a sensitive room, it cannot be made world readable \
							 (public room history)."
						)));
					}
				}
			}
		},
		| StateEventType::RoomCanonicalAlias => {
			if let Ok(canonical_alias) =
				serde_json::from_str::<RoomCanonicalAliasEventContent>(json.json().get())
			{
				let mut aliases = canonical_alias.alt_aliases.clone();

				if let Some(alias) = canonical_alias.alias {
					aliases.push(alias);
				}

				for alias in aliases {
					if !services.globals.server_is_ours(alias.server_name()) {
						return Err!(Request(Forbidden(
							"canonical_alias must be for this server"
						)));
					}

					if !services
						.rooms
						.alias
						.resolve_local_alias(&alias)
						.await
						.is_ok_and(|room| room == room_id)
					// Make sure it's the right room
					{
						return Err!(Request(Forbidden(
							"You are only allowed to send canonical_alias events when its \
							 aliases already exist"
						)));
					}
				}
			}
		},
		| StateEventType::RoomMember => {
			let Ok(membership_content) =
				serde_json::from_str::<RoomMemberEventContent>(json.json().get())
			else {
				return Err!(Request(BadJson(
					"Membership content must have a valid JSON body with at least a valid \
					 membership state."
				)));
			};

			let Ok(state_key) = UserId::parse(state_key) else {
				return Err!(Request(BadJson(
					"Membership event has invalid or non-existent state key"
				)));
			};

			if let Some(authorising_user) = membership_content.join_authorized_via_users_server {
				if membership_content.membership != MembershipState::Join {
					return Err!(Request(BadJson(
						"join_authorised_via_users_server is only for member joins"
					)));
				}

				if services
					.rooms
					.state_cache
					.is_joined(state_key, room_id)
					.await
				{
					return Err!(Request(InvalidParam(
						"{state_key} is already joined, an authorising user is not required."
					)));
				}

				if !services.globals.user_is_local(&authorising_user) {
					return Err!(Request(InvalidParam(
						"Authorising user {authorising_user} does not belong to this homeserver"
					)));
				}

				if !services
					.rooms
					.state_cache
					.is_joined(&authorising_user, room_id)
					.await
				{
					return Err!(Request(InvalidParam(
						"Authorising user {authorising_user} is not in the room, they cannot \
						 authorise the join."
					)));
				}
			}
		},
		| _ => (),
	}

	Ok(())
}
