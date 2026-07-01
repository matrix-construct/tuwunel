use axum::extract::State;
use futures::TryFutureExt;
use ruma::{
	api::{
		error::{ErrorKind, IncompatibleRoomVersionErrorData},
		federation::membership::prepare_knock_event,
	},
	events::room::member::{MembershipState, RoomMemberEventContent},
};
use tuwunel_core::{
	Err, Error, Result, at, debug_warn,
	matrix::{pdu::PduBuilder, room_version},
};

use super::utils::require_known_room;
use crate::Ruma;

/// # `GET /_matrix/federation/v1/make_knock/{roomId}/{userId}`
///
/// Creates a knock template.
pub(crate) async fn create_knock_event_template_route(
	State(services): State<crate::State>,
	body: Ruma<prepare_knock_event::v1::Request>,
) -> Result<prepare_knock_event::v1::Response> {
	require_known_room(&services, &body.room_id, body.origin()).await?;

	if body.user_id.server_name() != body.origin() {
		return Err!(Request(BadJson("Not allowed to knock on behalf of another server/user.")));
	}

	if let Some(server) = body.room_id.server_name()
		&& services
			.config
			.is_forbidden_remote_server_name(server)
	{
		return Err!(Request(Forbidden("Server is banned on this homeserver.")));
	}

	let room_version = services
		.state
		.get_room_version(&body.room_id)
		.await?;

	if !body.ver.contains(&room_version) {
		return Err(Error::BadRequest(
			ErrorKind::IncompatibleRoomVersion(IncompatibleRoomVersionErrorData::new(
				room_version,
			)),
			"Your homeserver does not support the features required to knock on this room.",
		));
	}

	let room_version_rules = room_version::rules(&room_version)?;

	if !room_version_rules.authorization.knocking {
		return Err(Error::BadRequest(
			ErrorKind::IncompatibleRoomVersion(IncompatibleRoomVersionErrorData::new(
				room_version,
			)),
			"Room version does not support knocking.",
		));
	}

	let state_lock = services.state.mutex.lock(&body.room_id).await;

	if let Ok(membership) = services
		.state_accessor
		.get_member(&body.room_id, &body.user_id)
		.await && membership.membership == MembershipState::Ban
	{
		debug_warn!(
			"Remote user {} is banned from {} but attempted to knock",
			&body.user_id,
			&body.room_id
		);

		return Err!(Request(Forbidden("You cannot knock on a room you are banned from.")));
	}

	let pdu_json = services
		.timeline
		.create_hash_and_sign_event(
			PduBuilder::state(
				body.user_id.to_string(),
				&RoomMemberEventContent::new(MembershipState::Knock),
			),
			&body.user_id,
			&body.room_id,
			&state_lock,
		)
		.map_ok(at!(1))
		.await?;

	drop(state_lock);

	let event = services
		.federation
		.format_pdu_into(pdu_json, Some(&room_version))
		.await;

	// room v3 and above removed the "event_id" field from remote PDU format
	Ok(prepare_knock_event::v1::Response { room_version, event })
}
