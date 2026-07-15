use ruma::{OwnedRoomOrAliasId, OwnedUserId};
use tuwunel_core::{Err, Result, debug_warn};
use tuwunel_service::membership::Join;

use super::BULK_JOIN_REASON;
use crate::{admin_command, utils::parse_active_local_user_id};

#[admin_command]
pub(super) async fn force_join_list_of_local_users(
	&self,
	room: OwnedRoomOrAliasId,
	yes_i_want_to_do_this: bool,
) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.",);
	}

	if !yes_i_want_to_do_this {
		return Err!(
			"You must pass the --yes-i-want-to-do-this flag to ensure you really want to force \
			 bulk join all specified local users.",
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

	let usernames = self
		.body
		.to_vec()
		.drain(1..self.body.len().saturating_sub(1))
		.collect::<Vec<_>>();

	let mut user_ids: Vec<OwnedUserId> = Vec::with_capacity(usernames.len());

	for username in usernames {
		match parse_active_local_user_id(self.services, username).await {
			| Ok(user_id) => {
				// don't make the server service account join
				if user_id == self.services.globals.server_user {
					self.services
						.admin
						.send_text(&format!(
							"{username} is the server service account, skipping over"
						))
						.await;

					continue;
				}

				user_ids.push(user_id);
			},
			| Err(e) => {
				self.services
					.admin
					.send_text(&format!("{username} is not a valid username, skipping over: {e}"))
					.await;

				continue;
			},
		}
	}

	let mut failed_joins: usize = 0;
	let mut successful_joins: usize = 0;

	for user_id in user_ids {
		match self
			.services
			.membership
			.join(Join {
				sender_user: &user_id,
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
