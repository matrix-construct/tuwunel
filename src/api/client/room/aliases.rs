use axum::extract::State;
use futures::StreamExt;
use ruma::api::client::room::aliases;
use tuwunel_core::{Err, Result};

use crate::Ruma;

/// # `GET /_matrix/client/r0/rooms/{roomId}/aliases`
///
/// Lists all aliases of the room.
///
/// - Only users joined to the room are allowed to call this, or if
///   `history_visibility` is world readable in the room
pub(crate) async fn get_room_aliases_route(
	State(services): State<crate::State>,
	body: Ruma<aliases::v3::Request>,
) -> Result<aliases::v3::Response> {
	let sender_user = body.sender_user();

	if !services
		.state_accessor
		.user_can_access_room(sender_user, &body.room_id)
		.await
	{
		return Err!(Request(Forbidden("You don't have permission to view this room.",)));
	}

	Ok(aliases::v3::Response {
		aliases: services
			.alias
			.local_aliases_for_room(&body.room_id)
			.map(ToOwned::to_owned)
			.collect()
			.await,
	})
}
