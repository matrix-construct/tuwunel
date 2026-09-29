use std::borrow::Cow;

use futures::StreamExt;
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use tuwunel_core::{Err, Result, err, utils::BoolExt};
use tuwunel_service::Services;

const UNFEDERATABLE_CAVEAT: &str = ", but this room's `m.room.create` event sets `m.federate` \
                                    to false, so remote users still cannot join it or be \
                                    invited to it. That property is fixed when the room is \
                                    created and no command can change it.";

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

/// Builds the reply for a command that re-enables a room's inbound
/// federation handling.
///
/// A room whose create event sets `m.federate` to false still cannot
/// federate afterward, so the reply for such a room extends `prefix` with
/// that caveat rather than confirming with `confirmation`. `confirmation`
/// is a complete sentence; `prefix` is a fragment the caveat continues.
pub(crate) async fn room_enabled_reply(
	services: &Services,
	room_id: &RoomId,
	confirmation: &'static str,
	prefix: &'static str,
) -> Cow<'static, str> {
	services
		.state_accessor
		.is_federating(room_id)
		.await
		.map_or_else(|| format!("{prefix}{UNFEDERATABLE_CAVEAT}").into(), || confirmation.into())
}

/// Parses user ID that is an active (not guest or deactivated) local user.
///
/// An account with an empty password reads as deactivated, which is how a
/// guest is refused along with a deactivated account.
pub(crate) async fn parse_active_local_user_id(
	services: &Services,
	user_id: &str,
) -> Result<OwnedUserId> {
	let user_id = parse_existing_local_user_id(services, user_id).await?;

	if services.users.is_deactivated(&user_id).await? {
		return Err!("User {user_id:?} is deactivated.");
	}

	Ok(user_id)
}

/// Parses user ID as our local user holding an account, deactivated or not.
///
/// A command writing for an absent user would create the account or leave
/// state behind for whoever registers the name later, so it refuses here.
pub(crate) async fn parse_existing_local_user_id(
	services: &Services,
	user_id: &str,
) -> Result<OwnedUserId> {
	let user_id = parse_user_id(services, user_id)?;

	check_existing_local_user(services, &user_id).await?;

	Ok(user_id)
}

/// Parses user ID as our local user.
///
/// The account need not exist; a command that requires one takes
/// `parse_existing_local_user_id` instead.
pub(crate) fn parse_local_user_id(services: &Services, user_id: &str) -> Result<OwnedUserId> {
	let user_id = parse_user_id(services, user_id)?;

	if !services.globals.user_is_local(&user_id) {
		return Err!("User {user_id:?} does not belong to our server.");
	}

	Ok(user_id)
}

/// Parses user ID.
///
/// The input is lowercased, and a bare localpart resolves against our server
/// name.
pub(crate) fn parse_user_id(services: &Services, user_id: &str) -> Result<OwnedUserId> {
	UserId::parse_with_server_name(user_id.to_lowercase(), services.globals.server_name())
		.map_err(|e| err!("The supplied username is not a valid username: {e}"))
}

/// Refuses a user unless they are local and hold an account.
///
/// The remote counterpart is `check_known_remote_user`.
pub(crate) async fn check_existing_local_user(services: &Services, user_id: &UserId) -> Result {
	if !services.globals.user_is_local(user_id) {
		return Err!("User {user_id:?} does not belong to our server.");
	}

	if !services.users.exists(user_id).await {
		return Err!("User {user_id:?} does not exist on this server.");
	}

	Ok(())
}

pub(crate) async fn check_known_remote_user(services: &Services, user_id: &UserId) -> Result {
	if services.globals.user_is_local(user_id) {
		return Err!("User {user_id:?} belongs to our server.");
	}

	if !services.users.exists(user_id).await {
		return Err!("User {user_id:?} is not known to this server.");
	}

	Ok(())
}
