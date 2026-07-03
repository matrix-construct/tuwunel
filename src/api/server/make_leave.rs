use axum::extract::State;
use futures::TryFutureExt;
use ruma::{
	api::federation::membership::prepare_leave_event,
	events::room::member::{MembershipState, RoomMemberEventContent},
};
use tuwunel_core::{Err, Result, at, matrix::pdu::PduBuilder};

use super::utils::require_known_room;
use crate::Ruma;

/// # `GET /_matrix/federation/v1/make_leave/{roomId}/{eventId}`
///
/// Creates a leave template.
pub(crate) async fn create_leave_event_template_route(
	State(services): State<crate::State>,
	body: Ruma<prepare_leave_event::v1::Request>,
) -> Result<prepare_leave_event::v1::Response> {
	require_known_room(&services, &body.room_id, body.origin()).await?;

	if body.user_id.server_name() != body.origin() {
		return Err!(Request(Forbidden(
			"Not allowed to leave on behalf of another server/user."
		)));
	}

	let room_version = services
		.state
		.get_room_version(&body.room_id)
		.map_ok(Some)
		.await?;

	let state_lock = services.state.mutex.lock(&body.room_id).await;

	let pdu_json = services
		.timeline
		.create_hash_and_sign_event(
			PduBuilder::state(
				body.user_id.to_string(),
				&RoomMemberEventContent::new(MembershipState::Leave),
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
		.format_pdu_into(pdu_json, room_version.as_ref())
		.await;

	Ok(prepare_leave_event::v1::Response { room_version, event })
}
