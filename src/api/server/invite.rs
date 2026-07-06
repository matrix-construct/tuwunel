use axum::extract::State;
use base64::{Engine as _, engine::general_purpose};
use futures::StreamExt;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId,
	ServerName, UserId,
	api::{
		appservice::event::push_events,
		error::{ErrorKind, IncompatibleRoomVersionErrorData},
		federation::membership::create_invite,
	},
	events::{
		AnyStrippedStateEvent, GlobalAccountDataEventType, StateEventType,
		push_rules::PushRulesEvent,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	push,
	serde::{JsonObject, Raw},
};
use tuwunel_core::{
	Err, Error, Result, debug_warn, err,
	matrix::{Event, PduCount, PduEvent, event::gen_event_id},
	utils,
	utils::hash::sha256,
};
use tuwunel_service::{
	Services,
	membership::{
		StrippedCreateVerdict, enforce_stripped_create, into_client_stripped, v12_room_ids,
	},
};

use crate::{ClientIp, Ruma};

/// # `PUT /_matrix/federation/v2/invite/{roomId}/{eventId}`
///
/// Invites a remote user to a room.
#[tracing::instrument(skip_all, fields(%client), name = "invite")]
pub(crate) async fn create_invite_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<create_invite::v2::Request>,
) -> Result<create_invite::v2::Response> {
	services
		.sending
		.notify_peer_alive(body.origin())
		.await;

	validate_request(&services, &body).await?;

	enforce_stripped_state(&services, &body).await?;

	let (mut signed_event, invited_user) = parse_and_validate_event(&services, &body).await?;

	sign_event(&services, &mut signed_event, &body.room_version)?;

	let sender = validate_origins(&signed_event, body.origin())?;

	check_invite_permitted(&services, &body, &invited_user).await?;

	let pdu = build_pdu(&body)?;

	let invite_state: Vec<_> = body
		.invite_room_state
		.clone()
		.into_iter()
		.filter_map(|state| into_client_stripped(&body.room_id, state))
		.chain([pdu.to_format()])
		.collect();

	// Block on the inbound /send applying the departure that removes our last
	// member, so the residency check observes it rather than stale state.
	let _federation_lock = services
		.event_handler
		.mutex_federation
		.lock(&body.room_id)
		.await;

	if !services
		.state_cache
		.server_in_room(services.globals.server_name(), &body.room_id)
		.await
	{
		record_local_invite(&services, &body, &invited_user, sender, invite_state, &pdu).await?;
	}

	Ok(create_invite::v2::Response {
		event: services
			.federation
			.format_pdu_into(signed_event, Some(&body.room_version))
			.await,
	})
}

async fn validate_request(
	services: &Services,
	body: &Ruma<create_invite::v2::Request>,
) -> Result<()> {
	services
		.event_handler
		.acl_check(body.origin(), &body.room_id)
		.await?;

	if !services
		.config
		.supported_room_version(&body.room_version)
	{
		return Err(Error::BadRequest(
			ErrorKind::IncompatibleRoomVersion(IncompatibleRoomVersionErrorData::new(
				body.room_version.clone(),
			)),
			"Server does not support this room version.",
		));
	}

	if let Some(server) = body.room_id.server_name()
		&& services
			.config
			.is_forbidden_remote_server_name(server)
	{
		return Err!(Request(Forbidden("Server is banned on this homeserver.")));
	}

	Ok(())
}

/// Validate the create event in the invite's stripped state (MSC4311) and
/// reject the invite when the operator's policy requires it.
async fn enforce_stripped_state(
	services: &Services,
	body: &Ruma<create_invite::v2::Request>,
) -> Result<()> {
	let verdict = services
		.membership
		.validate_stripped_create(&body.invite_room_state, &body.room_id, &body.room_version)
		.await?;

	if verdict != StrippedCreateVerdict::Valid {
		debug_warn!(
			?verdict,
			room_id = %body.room_id,
			"MSC4311 invite create-event validation failed",
		);
	}

	if enforce_stripped_create(
		verdict,
		v12_room_ids(&body.room_version),
		services
			.config
			.enforce_stripped_state_pdu_validation,
	) {
		return Err!(Request(MissingParam(
			"The invite's m.room.create event is missing or does not validate for this room."
		)));
	}

	Ok(())
}

async fn parse_and_validate_event(
	services: &Services,
	body: &Ruma<create_invite::v2::Request>,
) -> Result<(CanonicalJsonObject, OwnedUserId)> {
	let signed_event = utils::to_canonical_object(&body.event)
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

	if !services.globals.user_is_local(&invited_user) {
		return Err!(Request(InvalidParam("User does not belong to this homeserver.")));
	}

	if services
		.users
		.invites_blocked(&invited_user)
		.await
	{
		return Err!(Request(InviteBlocked("{invited_user} has blocked invites.")));
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

	services
		.event_handler
		.acl_check(invited_user.server_name(), &body.room_id)
		.await?;

	Ok((signed_event, invited_user))
}

fn sign_event(
	services: &Services,
	signed_event: &mut CanonicalJsonObject,
	room_version: &RoomVersionId,
) -> Result<()> {
	services
		.server_keys
		.hash_and_sign_event(signed_event, room_version)
		.map_err(|e| err!(Request(InvalidParam("Failed to sign event: {e}"))))?;

	let event_id = gen_event_id(signed_event, room_version)?;
	signed_event.insert("event_id".into(), CanonicalJsonValue::String(event_id.to_string()));

	Ok(())
}

fn validate_origins<'a>(
	signed_event: &'a CanonicalJsonObject,
	body_origin: &ServerName,
) -> Result<&'a UserId> {
	let origin: Option<&str> = signed_event
		.get("origin")
		.and_then(CanonicalJsonValue::as_str);

	let sender: &UserId = signed_event
		.get("sender")
		.try_into()
		.map_err(|e| err!(Request(InvalidParam("Invalid sender property: {e}"))))?;

	if sender.server_name() != body_origin {
		return Err!(Request(Forbidden("Can only send invites on behalf of your users.")));
	}

	if origin.is_some_and(|origin| origin != body_origin) {
		return Err!(Request(Forbidden("Can only send events from your origin.")));
	}

	Ok(sender)
}

async fn check_invite_permitted(
	services: &Services,
	body: &Ruma<create_invite::v2::Request>,
	invited_user: &UserId,
) -> Result<()> {
	if services.metadata.is_banned(&body.room_id).await
		&& !services.admin.user_is_admin(invited_user).await
	{
		return Err!(Request(Forbidden("This room is banned on this homeserver.")));
	}

	if services.config.block_non_admin_invites
		&& !services.admin.user_is_admin(invited_user).await
	{
		return Err!(Request(Forbidden("This server does not allow room invites.")));
	}

	Ok(())
}

fn build_pdu(body: &Ruma<create_invite::v2::Request>) -> Result<PduEvent> {
	let mut event: JsonObject = serde_json::from_str(body.event.get())
		.map_err(|e| err!(Request(BadJson("Invalid invite event PDU: {e}"))))?;

	event.insert("event_id".into(), "$placeholder".into());

	serde_json::from_value(event.into())
		.map_err(|e| err!(Request(BadJson("Invalid invite event PDU: {e}"))))
}

/// Record an invite for a room we are not currently in.
///
/// When we are active in the room, the remote server will notify us about the
/// join/invite through `/send`. When we are not in the room, the invited state
/// must be recorded manually for client `/sync` through `update_membership()`,
/// and the invite PDU pushed to the relevant appservices.
async fn record_local_invite(
	services: &Services,
	body: &Ruma<create_invite::v2::Request>,
	invited_user: &UserId,
	sender: &UserId,
	invite_state: Vec<Raw<AnyStrippedStateEvent>>,
	pdu: &PduEvent,
) -> Result<()> {
	if services
		.state_accessor
		.room_state_get_content::<RoomMemberEventContent>(
			&body.room_id,
			&StateEventType::RoomMember,
			invited_user.as_str(),
		)
		.await
		.is_ok_and(|content| content.membership == MembershipState::Ban)
	{
		debug_warn!(
			room_id = %body.room_id,
			user_id = %invited_user,
			"Recording invite while local room state shows banned membership.",
		);
	}

	let count = services.globals.next_count();
	services
		.state_cache
		.update_membership(
			&body.room_id,
			invited_user,
			RoomMemberEventContent::new(MembershipState::Invite),
			sender,
			Some(invite_state),
			body.via.clone(),
			true,
			PduCount::Normal(*count),
		)
		.await?;
	drop(count);

	notify_pushers(services, invited_user, pdu).await;

	for appservice in services.appservice.read().await.values() {
		if appservice.is_user_match(invited_user) {
			services
				.appservice
				.send_request(appservice.registration.clone(), push_events::v1::Request {
					events: vec![pdu.to_format()],
					txn_id: general_purpose::URL_SAFE_NO_PAD
						.encode(sha256::hash(pdu.event_id.as_bytes()))
						.into(),
					ephemeral: Vec::new(),
					to_device: Vec::new(),
				})
				.await?;
		}
	}

	Ok(())
}

async fn notify_pushers(services: &Services, invited_user: &UserId, pdu: &PduEvent) {
	services
		.pusher
		.get_pushkeys(invited_user)
		.map(ToOwned::to_owned)
		.for_each(async |pushkey| {
			let Ok(pusher) = services
				.pusher
				.get_pusher(invited_user, &pushkey)
				.await
			else {
				return;
			};

			let ruleset = services
				.account_data
				.get_global(invited_user, GlobalAccountDataEventType::PushRules)
				.await
				.map_or_else(
					|_| push::Ruleset::server_default(invited_user),
					|ev: PushRulesEvent| ev.content.global,
				);

			services
				.pusher
				.send_push_notice(invited_user, &pusher, &ruleset, pdu)
				.await
				.ok();
		})
		.await;
}
