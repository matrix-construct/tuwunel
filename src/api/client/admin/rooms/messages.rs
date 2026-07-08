use axum::extract::State;
use ruma::api::{Direction, client::filter::RoomEventFilter};
use synapse_admin_api::rooms::admin_messages::v1::{Request, Response};
use tuwunel_core::Result;

use crate::{
	Ruma,
	client::{
		admin::require_admin,
		message::{MessagesArgs, get_messages},
	},
};

/// # `GET /_synapse/admin/v1/rooms/{room_id}/messages`
///
/// Paginates a room's timeline with admin privilege, skipping the membership
/// gate and the per-event visibility and ignore filters the client-server
/// endpoint applies.
pub(crate) async fn admin_room_messages_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let filter: RoomEventFilter = body
		.filter
		.as_deref()
		.map(serde_json::from_str)
		.transpose()?
		.unwrap_or_default();

	let response = get_messages(&services, MessagesArgs {
		room_id: &body.room_id,
		sender_user: body.sender_user(),
		sender_device: body.sender_device.as_deref(),
		from: body.from.as_deref(),
		to: body.to.as_deref(),
		dir: body.dir.unwrap_or(Direction::Forward),
		limit: body.limit,
		filter: &filter,
		bypass_visibility: true,
	})
	.await?;

	Ok(Response {
		start: response.start,
		end: response.end,
		chunk: response.chunk,
		state: response.state,
	})
}
