use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use base64::{Engine as _, engine::general_purpose};
use futures::StreamExt;
use ruma::{
	CanonicalJsonValue, OwnedRoomId, OwnedUserId, RoomId, UserId,
	api::{
		client::error::ErrorKind,
		federation::membership::{RawStrippedState, create_invite},
	},
	events::{
		GlobalAccountDataEventType, StateEventType,
		push_rules::PushRulesEvent,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	push,
	serde::JsonObject,
};
use tuwunel_core::{
	Err, Error, Result, err, extract_variant,
	matrix::{Event, PduCount, PduEvent, event::gen_event_id},
	utils,
	utils::hash::sha256,
};

use crate::Ruma;

/// # `PUT /_matrix/federation/v2/invite/{roomId}/{eventId}`
///
/// Invites a remote user to a room.
#[tracing::instrument(skip_all, fields(%client), name = "invite")]
pub(crate) async fn create_invite_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<create_invite::v2::Request>,
) -> Result<create_invite::v2::Response> {
	// ACL check origin
	services
		.event_handler
		.acl_check(body.origin(), &body.room_id)
		.await?;

	if !services
		.config
		.supported_room_version(&body.room_version)
	{
		return Err(Error::BadRequest(
			ErrorKind::IncompatibleRoomVersion { room_version: body.room_version.clone() },
			"Server does not support this room version.",
		));
	}

	if let Some(server) = body.room_id.server_name()
		&& services
			.config
			.forbidden_remote_server_names
			.is_match(server.host())
	{
		return Err!(Request(Forbidden("Server is banned on this homeserver.")));
	}

	let mut signed_event = utils::to_canonical_object(&body.event)
		.map_err(|_| err!(Request(InvalidParam("Invite event is invalid."))))?;

	let room_id: OwnedRoomId = signed_event
		.get("room_id")
		.try_into()
		.map(RoomId::to_owned)
		.map_err(|e| err!(Request(InvalidParam("Invalid room_id property: {e}"))))?;

	if body.room_id != room_id {
		return Err!(Request(InvalidParam("Event room_id does not match the request path.")));
	}

	let kind: StateEventType = signed_event
		.get("type")
		.and_then(CanonicalJsonValue::as_str)
		.ok_or_else(|| err!(Request(BadJson("Missing type in event."))))?
		.into();

	if kind != StateEventType::RoomMember {
		return Err!(Request(InvalidParam("Event must be m.room.member type.")));
	}

	let invited_user: OwnedUserId = signed_event
		.get("state_key")
		.try_into()
		.map(UserId::to_owned)
		.map_err(|e| err!(Request(InvalidParam("Invalid state_key property: {e}"))))?;

	if !services
		.globals
		.server_is_ours(invited_user.server_name())
	{
		return Err!(Request(InvalidParam("User does not belong to this homeserver.")));
	}

	let content: RoomMemberEventContent = signed_event
		.get("content")
		.cloned()
		.map(Into::into)
		.map(serde_json::from_value)
		.transpose()
		.map_err(|e| err!(Request(InvalidParam("Invalid content object in event: {e}"))))?
		.ok_or_else(|| err!(Request(BadJson("Missing content in event."))))?;

	if content.membership != MembershipState::Invite {
		return Err!(Request(InvalidParam("Event membership must be invite.")));
	}

	// Make sure we're not ACL'ed from their room.
	services
		.event_handler
		.acl_check(invited_user.server_name(), &body.room_id)
		.await?;

	services
		.server_keys
		.hash_and_sign_event(&mut signed_event, &body.room_version)
		.map_err(|e| err!(Request(InvalidParam("Failed to sign event: {e}"))))?;

	// Generate event id
	let event_id = gen_event_id(&signed_event, &body.room_version)?;

	// Add event_id back
	signed_event.insert("event_id".into(), CanonicalJsonValue::String(event_id.to_string()));

	let origin: Option<&str> = signed_event
		.get("origin")
		.and_then(CanonicalJsonValue::as_str);

	let sender: &UserId = signed_event
		.get("sender")
		.try_into()
		.map_err(|e| err!(Request(InvalidParam("Invalid sender property: {e}"))))?;

	if sender.server_name() != body.origin() {
		return Err!(Request(Forbidden("Can only send invites on behalf of your users.")));
	}

	if origin.is_some_and(|origin| origin != sender.server_name()) {
		return Err!(Request(Forbidden("Your users can only be from your origin.")));
	}

	if origin.is_some_and(|origin| origin != body.origin()) {
		return Err!(Request(Forbidden("Can only send events from your origin.")));
	}

	if services.metadata.is_banned(&body.room_id).await
		&& !services.admin.user_is_admin(&invited_user).await
	{
		return Err!(Request(Forbidden("This room is banned on this homeserver.")));
	}

	if services.config.block_non_admin_invites
		&& !services.admin.user_is_admin(&invited_user).await
	{
		return Err!(Request(Forbidden("This server does not allow room invites.")));
	}

	let mut invite_state: Vec<_> = body
		.invite_room_state
		.clone()
		.into_iter()
		.filter_map(|s| extract_variant!(s, RawStrippedState::Stripped))
		.collect();

	let mut event: JsonObject = serde_json::from_str(body.event.get())
		.map_err(|e| err!(Request(BadJson("Invalid invite event PDU: {e}"))))?;

	event.insert("event_id".into(), "$placeholder".into());

	let pdu: PduEvent = serde_json::from_value(event.into())
		.map_err(|e| err!(Request(BadJson("Invalid invite event PDU: {e}"))))?;

	invite_state.push(pdu.to_format());

	// If we are active in the room, the remote server will notify us about the
	// join/invite through /send. If we are not in the room, we need to manually
	// record the invited state for client /sync through update_membership(), and
	// send the invite PDU to the relevant appservices.
	if !services
		.state_cache
		.server_in_room(services.globals.server_name(), &body.room_id)
		.await
	{
		let count = services.globals.next_count();
		services
			.state_cache
			.update_membership(
				&body.room_id,
				&invited_user,
				RoomMemberEventContent::new(MembershipState::Invite),
				sender,
				Some(invite_state),
				body.via.clone(),
				true,
				PduCount::Normal(*count),
			)
			.await?;
		drop(count);

		services
			.pusher
			.get_pushkeys(&invited_user)
			.map(ToOwned::to_owned)
			.for_each(async |pushkey| {
				let Ok(pusher) = services
					.pusher
					.get_pusher(&invited_user, &pushkey)
					.await
				else {
					return;
				};

				let ruleset = services
					.account_data
					.get_global(&invited_user, GlobalAccountDataEventType::PushRules)
					.await
					.map_or_else(
						|_| push::Ruleset::server_default(&invited_user),
						|ev: PushRulesEvent| ev.content.global,
					);

				services
					.pusher
					.send_push_notice(&invited_user, &pusher, &ruleset, &pdu)
					.await
					.ok();
			})
			.await;

		for appservice in services.appservice.read().await.values() {
			if appservice.is_user_match(&invited_user) {
				services
					.appservice
					.send_request(
						appservice.registration.clone(),
						ruma::api::appservice::event::push_events::v1::Request {
							events: vec![pdu.to_format()],
							txn_id: general_purpose::URL_SAFE_NO_PAD
								.encode(sha256::hash(pdu.event_id.as_bytes()))
								.into(),
							ephemeral: Vec::new(),
							to_device: Vec::new(),
						},
					)
					.await?;
			}
		}
	}

	Ok(create_invite::v2::Response {
		event: services
			.federation
			.format_pdu_into(signed_event, Some(&body.room_version))
			.await,
	})
}
