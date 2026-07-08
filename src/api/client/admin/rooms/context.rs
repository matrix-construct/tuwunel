use axum::extract::State;
use ruma::api::client::filter::RoomEventFilter;
use synapse_admin_api::rooms::admin_context::v1::{Request, Response};
use tuwunel_core::Result;

use crate::{
	Ruma,
	client::{
		admin::require_admin,
		context::{ContextArgs, event_context},
	},
};

/// # `GET /_synapse/admin/v1/rooms/{room_id}/context/{event_id}`
///
/// Loads the timeline window around an event with admin privilege, skipping the
/// visibility and ignore checks the client-server context endpoint applies.
pub(crate) async fn admin_room_context_route(
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

	let response = event_context(&services, ContextArgs {
		room_id: &body.room_id,
		event_id: &body.event_id,
		sender_user: body.sender_user(),
		sender_device: body.sender_device.as_deref(),
		filter: &filter,
		limit: body.limit,
		bypass_visibility: true,
	})
	.await?;

	Ok(Response {
		start: response.start.unwrap_or_default(),
		end: response.end.unwrap_or_default(),
		events_before: response.events_before,
		event: response.event,
		events_after: response.events_after,
		state: response.state,
	})
}
