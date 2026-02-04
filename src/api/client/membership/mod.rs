mod ban;
mod forget;
mod invite;
mod join;
mod kick;
mod knock;
mod leave;
mod members;
mod unban;

use std::net::IpAddr;

use axum::extract::State;
use futures::{FutureExt, StreamExt};
use ruma::{RoomId, RoomOrAliasId, UserId, api::client::membership::joined_rooms};
use tuwunel_core::{Err, Result, result::LogErr, warn};
use tuwunel_service::Services;

pub(crate) use self::{
	ban::ban_user_route,
	forget::forget_room_route,
	invite::invite_user_route,
	join::{join_room_by_id_or_alias_route, join_room_by_id_route},
	kick::kick_user_route,
	knock::knock_room_route,
	leave::leave_room_route,
	members::{get_member_events_route, joined_members_route},
	unban::unban_user_route,
};
use crate::Ruma;

/// # `POST /_matrix/client/r0/joined_rooms`
///
/// Lists all rooms the user has joined.
pub(crate) async fn joined_rooms_route(
	State(services): State<crate::State>,
	body: Ruma<joined_rooms::v3::Request>,
) -> Result<joined_rooms::v3::Response> {
	Ok(joined_rooms::v3::Response {
		joined_rooms: services
			.state_cache
			.rooms_joined(body.sender_user())
			.map(ToOwned::to_owned)
			.collect()
			.await,
	})
}

/// Checks if the room is banned in any way possible and the sender user is not
/// an admin.
///
/// Performs automatic deactivation if `auto_deactivate_banned_room_attempts` is
/// enabled
#[tracing::instrument(skip(services))]
pub(crate) async fn banned_room_check(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	orig_room_id: Option<&RoomOrAliasId>,
	client_ip: IpAddr,
) -> Result {
	if services.admin.user_is_admin(user_id).await {
		return Ok(());
	}

	// room id is banned ...
	if services.metadata.is_banned(room_id).await
		// ... or legacy room id server is banned ...
		|| room_id.server_name().is_some_and(|server_name| {
			services
				.config
				.forbidden_remote_server_names
				.is_match(server_name.host())
		})
		// ... or alias server is banned
		|| orig_room_id.is_some_and(|orig_room_id| {
			orig_room_id.server_name().is_some_and(|orig_server_name|
			services
				.config
				.forbidden_remote_server_names
				.is_match(orig_server_name.host()))
	}) {
		warn!(
			"User {user_id} who is not an admin attempted to send an invite for or attempted to \
			 join a banned room or banned room server name: {room_id}"
		);

		maybe_deactivate(services, user_id, client_ip)
			.await
			.log_err()
			.ok();

		return Err!(Request(Forbidden("This room is banned on this homeserver.")));
	}

	Ok(())
}

async fn maybe_deactivate(services: &Services, user_id: &UserId, client_ip: IpAddr) -> Result {
	if services
		.server
		.config
		.auto_deactivate_banned_room_attempts
	{
		let notice = format!(
			"Automatically deactivating user {user_id} due to attempted banned room join from \
			 IP {client_ip}"
		);

		warn!("{notice}");

		if services.server.config.admin_room_notices {
			services.admin.send_text(&notice).await;
		}

		services
			.deactivate
			.full_deactivate(user_id)
			.boxed()
			.await?;
	}

	Ok(())
}
