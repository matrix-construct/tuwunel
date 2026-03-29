use futures::StreamExt;
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use tuwunel_core::{Err, Result, err};
use tuwunel_service::Services;

pub(crate) async fn get_room_info(
	services: &Services,
	room_id: &RoomId,
) -> (OwnedRoomId, u64, String) {
	let join_count = services
		.state_cache
		.room_joined_count(room_id)
		.await
		.unwrap_or(0);

	let name = match services.state_accessor.get_name(room_id).await {
		| Ok(name) => name,
		| Err(_) if join_count == 2 => services
			.state_cache
			.room_members(room_id)
			.map(ToString::to_string)
			.collect::<Vec<_>>()
			.await
			.join(", "),
		| Err(_) => room_id.to_string(),
	};

	(room_id.into(), join_count, name)
}

/// Parses user ID
pub(crate) fn parse_user_id(services: &Services, user_id: &str) -> Result<OwnedUserId> {
	UserId::parse_with_server_name(user_id.to_lowercase(), services.globals.server_name())
		.map_err(|e| err!("The supplied username is not a valid username: {e}"))
}

/// Parses user ID as our local user
pub(crate) fn parse_local_user_id(services: &Services, user_id: &str) -> Result<OwnedUserId> {
	let user_id = parse_user_id(services, user_id)?;

	if !services.globals.user_is_local(&user_id) {
		return Err!("User {user_id:?} does not belong to our server.");
	}

	Ok(user_id)
}

/// Parses user ID that is an active (not guest or deactivated) local user
pub(crate) async fn parse_active_local_user_id(
	services: &Services,
	user_id: &str,
) -> Result<OwnedUserId> {
	let user_id = parse_local_user_id(services, user_id)?;

	if !services.users.exists(&user_id).await {
		return Err!("User {user_id:?} does not exist on this server.");
	}

	if services.users.is_deactivated(&user_id).await? {
		return Err!("User {user_id:?} is deactivated.");
	}

	Ok(user_id)
}
