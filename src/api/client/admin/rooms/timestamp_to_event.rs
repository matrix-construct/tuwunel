use axum::extract::State;
use ruma::api::Direction;
use synapse_admin_api::rooms::admin_timestamp_to_event::v1::{Request, Response};
use tuwunel_core::Result;

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/rooms/{room_id}/timestamp_to_event`
///
/// Returns the event closest to the given timestamp, skipping the visibility
/// gates the client-server timestamp endpoint applies.
pub(crate) async fn admin_room_timestamp_to_event_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let dir = body.dir.unwrap_or(Direction::Forward);

	let (origin_server_ts, event_id) = services
		.timeline
		.get_event_id_near_ts_with_fallback(&body.room_id, body.ts, dir)
		.await?;

	Ok(Response::new(event_id, origin_server_ts))
}
