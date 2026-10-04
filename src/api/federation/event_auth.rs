use std::{borrow::Borrow, iter::once};

use axum::extract::State;
use futures::StreamExt;
use ruma::api::federation::authorization::get_event_authorization;
use tuwunel_core::{
	Result,
	utils::stream::{BroadbandExt, ReadyExt},
};

use super::{AccessCheck, utils::require_event_in_room};
use crate::Ruma;

/// # `GET /_matrix/federation/v1/event_auth/{roomId}/{eventId}`
///
/// Retrieves the auth chain for a given event.
///
/// - This does not include the event itself
pub(crate) async fn get_event_authorization_route(
	State(services): State<crate::State>,
	body: Ruma<get_event_authorization::v1::Request>,
) -> Result<get_event_authorization::v1::Response> {
	let access_check = AccessCheck {
		services: &services,
		origin: body.origin(),
		room_id: &body.room_id,
		event_id: None,
	};

	access_check.check().await?;

	require_event_in_room(&services, &body.event_id, &body.room_id).await?;

	let room_version = services
		.state
		.get_room_version(&body.room_id)
		.await?;

	let auth_chain = services
		.auth_chain
		.event_ids_iter(&body.room_id, &room_version, once(body.event_id.borrow()))
		.ready_filter_map(Result::ok)
		.broad_filter_map(async |id| {
			let pdu = services.timeline.get_pdu_json(&id).await.ok()?;

			let pdu = services
				.federation
				.format_pdu_into(pdu, Some(&room_version))
				.await;

			Some(pdu)
		})
		.collect()
		.await;

	Ok(get_event_authorization::v1::Response { auth_chain })
}
