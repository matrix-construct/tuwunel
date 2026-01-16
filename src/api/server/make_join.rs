use axum::extract::State;
use futures::{StreamExt, TryFutureExt, pin_mut};
use ruma::{
	OwnedUserId, RoomId, RoomVersionId, UserId,
	api::{client::error::ErrorKind, federation::membership::prepare_join_event},
	events::{
		StateEventType,
		room::{
			join_rules::{AllowRule, JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
		},
	},
};
use tuwunel_core::{
	Err, Error, Result, at, debug_info, matrix::pdu::PduBuilder, utils::IterStream,
};
use tuwunel_service::Services;

use crate::Ruma;

/// # `GET /_matrix/federation/v1/make_join/{roomId}/{userId}`
///
/// Creates a join template.
pub(crate) async fn create_join_event_template_route(
	State(services): State<crate::State>,
	body: Ruma<prepare_join_event::v1::Request>,
) -> Result<prepare_join_event::v1::Response> {
	if !services.metadata.exists(&body.room_id).await {
		return Err!(Request(NotFound("Room is unknown to this server.")));
	}

	if body.user_id.server_name() != body.origin() {
		return Err!(Request(BadJson("Not allowed to join on behalf of another server/user.")));
	}

	// ACL check origin server
	services
		.event_handler
		.acl_check(body.origin(), &body.room_id)
		.await?;

	if let Some(server) = body.room_id.server_name()
		&& services
			.config
			.forbidden_remote_server_names
			.is_match(server.host())
	{
		return Err!(Request(Forbidden(warn!(
			"Room ID server name {server} is banned on this homeserver."
		))));
	}

	let room_version_id = services
		.state
		.get_room_version(&body.room_id)
		.await?;

	if !body.ver.contains(&room_version_id) {
		return Err(Error::BadRequest(
			ErrorKind::IncompatibleRoomVersion { room_version: room_version_id },
			"Room version not supported.",
		));
	}

	let state_lock = services.state.mutex.lock(&body.room_id).await;

	let join_authorized_via_users_server: Option<OwnedUserId> = {
		use RoomVersionId::*;
		if matches!(room_version_id, V1 | V2 | V3 | V4 | V5 | V6 | V7) {
			// room version does not support restricted join rules
			None
		} else if user_can_perform_restricted_join(
			&services,
			&body.user_id,
			&body.room_id,
			&room_version_id,
		)
		.await?
		{
			let users = services
				.state_cache
				.local_users_in_room(&body.room_id)
				.filter(|user| {
					services.state_accessor.user_can_invite(
						&body.room_id,
						user,
						&body.user_id,
						&state_lock,
					)
				})
				.map(ToOwned::to_owned);

			pin_mut!(users);
			let Some(auth_user) = users.next().await else {
				return Err!(Request(UnableToGrantJoin(
					"No user on this server is able to assist in joining."
				)));
			};

			Some(auth_user)
		} else {
			None
		}
	};

	let pdu_json = services
		.timeline
		.create_hash_and_sign_event(
			PduBuilder::state(body.user_id.to_string(), &RoomMemberEventContent {
				join_authorized_via_users_server,
				..RoomMemberEventContent::new(MembershipState::Join)
			}),
			&body.user_id,
			&body.room_id,
			&state_lock,
		)
		.map_ok(at!(1))
		.await?;

	drop(state_lock);

	Ok(prepare_join_event::v1::Response {
		room_version: Some(room_version_id.clone()),
		event: services
			.federation
			.format_pdu_into(pdu_json, Some(&room_version_id))
			.await,
	})
}

/// Checks whether the given user can join the given room via a restricted join.
pub(crate) async fn user_can_perform_restricted_join(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
) -> Result<bool> {
	use RoomVersionId::*;

	// restricted rooms are not supported on <=v7
	if matches!(room_version_id, V1 | V2 | V3 | V4 | V5 | V6 | V7) {
		return Ok(false);
	}

	if services
		.state_cache
		.is_joined(user_id, room_id)
		.await
	{
		// joining user is already joined, there is nothing we need to do
		return Ok(false);
	}

	if services
		.state_cache
		.is_invited(user_id, room_id)
		.await
	{
		return Ok(true);
	}

	let Ok(join_rules_event_content) = services
		.state_accessor
		.room_state_get_content::<RoomJoinRulesEventContent>(
			room_id,
			&StateEventType::RoomJoinRules,
			"",
		)
		.await
	else {
		return Ok(false);
	};

	let (JoinRule::Restricted(r) | JoinRule::KnockRestricted(r)) =
		join_rules_event_content.join_rule
	else {
		return Ok(false);
	};

	if r.allow.is_empty() {
		debug_info!("{room_id} is restricted but the allow key is empty");
		return Ok(false);
	}

	if r.allow
		.iter()
		.filter_map(|rule| {
			if let AllowRule::RoomMembership(membership) = rule {
				Some(membership)
			} else {
				None
			}
		})
		.stream()
		.any(|m| {
			services
				.state_cache
				.is_joined(user_id, &m.room_id)
		})
		.await
	{
		Ok(true)
	} else {
		Err!(Request(UnableToAuthorizeJoin(
			"Joining user is not known to be in any required room."
		)))
	}
}
