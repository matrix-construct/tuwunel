use axum::extract::State;
use futures::{TryStreamExt, future::Either};
use ruma::events::StateEventType;
use synapse_admin_api::rooms::admin_state::v1::{Request, Response};
use tuwunel_core::{Result, matrix::Event, utils::stream::TryBroadbandExt};

use crate::{
	Ruma,
	client::{admin::require_admin, with_membership},
};

/// # `GET /_synapse/admin/v1/rooms/{room_id}/state`
///
/// Returns the room's current state, optionally filtered to a single event
/// type, bypassing the visibility gate the client-server state endpoint
/// applies.
pub(crate) async fn admin_room_state_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let sender_user = body.sender_user();

	require_admin(&services, sender_user).await?;

	let encrypted = services
		.state_accessor
		.is_encrypted_room(&body.room_id)
		.await;

	let event_type = body
		.event_type
		.as_deref()
		.map(StateEventType::from);

	let pdus = match &event_type {
		| Some(event_type) => Either::Left(
			services
				.state_accessor
				.room_state_type_pdus(&body.room_id, event_type)
				.map_ok(Event::into_pdu),
		),
		| None => Either::Right(
			services
				.state_accessor
				.room_state_full_pdus(&body.room_id)
				.map_ok(Event::into_pdu),
		),
	};

	let state = pdus
		.broad_and_then(async |pdu| {
			Ok(with_membership(&services, pdu, sender_user, encrypted).await)
		})
		.map_ok(Event::into_format)
		.try_collect()
		.await?;

	Ok(Response { state })
}
