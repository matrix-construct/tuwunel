use std::str::FromStr;

use axum::extract::State;
use ruma::{UInt, room::RoomSummary, serde::Raw};
use synapse_admin_api::rooms::admin_hierarchy::v1::{Request, Response};
use tuwunel_core::Result;
use tuwunel_service::rooms::spaces::PaginationToken;

use crate::{
	Ruma,
	client::{
		admin::require_admin,
		space::{HierarchyArgs, get_client_hierarchy},
	},
};

/// # `GET /_synapse/admin/v1/rooms/{room_id}/hierarchy`
///
/// Walks a space tree with admin privilege: never federates (remote children
/// surface as holes) and skips the per-user room-visibility check.
pub(crate) async fn admin_room_hierarchy_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let limit = body
		.limit
		.unwrap_or_else(|| UInt::from(50_u32))
		.min(UInt::from(50_u32));

	let max_depth = body
		.max_depth
		.and_then(|max_depth| max_depth.try_into().ok())
		.unwrap_or(usize::MAX);

	let skip_room_ids = body
		.from
		.as_deref()
		.and_then(|from| PaginationToken::from_str(from).ok())
		.map(|token| token.short_room_ids)
		.unwrap_or_default();

	let response = get_client_hierarchy(&services, HierarchyArgs {
		sender_user: body.sender_user(),
		room_id: &body.room_id,
		limit: limit.try_into().unwrap_or(50),
		max_depth,
		suggested_only: false,
		skip_room_ids: &skip_room_ids,
		bypass_visibility: true,
	})
	.await?;

	let rooms: Vec<Raw<RoomSummary>> = response
		.rooms
		.iter()
		.filter_map(|chunk| Raw::new(chunk).ok().map(Raw::cast_unchecked))
		.collect();

	Ok(Response { rooms, next_batch: response.next_batch })
}
