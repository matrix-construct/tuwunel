use futures::StreamExt;
use ruma::{OwnedRoomOrAliasId, UserId};
use tuwunel_core::{Err, Result, debug_warn};
use tuwunel_service::membership::Join;

use super::BULK_JOIN_REASON;
use crate::admin_command;

#[admin_command]
pub(super) async fn force_join_all_local_users(
	&self,
	room: OwnedRoomOrAliasId,
	yes_i_want_to_do_this: bool,
) -> Result {
	if !yes_i_want_to_do_this {
		return Err!(
			"You must pass the --yes-i-want-to-do-this-flag to ensure you really want to force \
			 bulk join all local users.",
		);
	}

	let (room_id, servers) = self
		.services
		.alias
		.maybe_resolve_with_servers(&room, None)
		.await?;

	if !self
		.services
		.state_cache
		.server_in_room(self.services.globals.server_name(), &room_id)
		.await
	{
		return Err!("We are not joined in this room.");
	}

	let mut failed_joins: usize = 0;
	let mut successful_joins: usize = 0;

	for user_id in &self
		.services
		.users
		.list_local_users()
		.map(UserId::to_owned)
		.collect::<Vec<_>>()
		.await
	{
		if user_id == &self.services.globals.server_user {
			continue;
		}

		match self
			.services
			.membership
			.join(Join {
				sender_user: user_id,
				room_id: &room_id,
				orig_room_id: Some(&room),
				reason: Some(String::from(BULK_JOIN_REASON)),
				servers: &servers,
				is_appservice: false,
				extra_content: None,
			})
			.await
		{
			| Ok(_res) => {
				successful_joins = successful_joins.saturating_add(1);
			},
			| Err(e) => {
				debug_warn!("Failed force joining {user_id} to {room_id} during bulk join: {e}");
				failed_joins = failed_joins.saturating_add(1);
			},
		}
	}

	write!(
		self,
		"{successful_joins} local users have been joined to {room_id}. {failed_joins} joins \
		 failed.",
	)
	.await
}
