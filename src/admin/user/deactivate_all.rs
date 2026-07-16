use ruma::OwnedUserId;
use tuwunel_core::{Err, Result};

use super::deactivate_user;
use crate::{admin_command, utils::parse_active_local_user_id};

#[admin_command]
pub(super) async fn deactivate_all(&self, no_leave_rooms: bool, force: bool) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.",);
	}

	let usernames = self
		.body
		.to_vec()
		.drain(1..self.body.len().saturating_sub(1))
		.collect::<Vec<_>>();

	let mut user_ids: Vec<OwnedUserId> = Vec::with_capacity(usernames.len());
	let mut admins = Vec::new();

	for username in usernames {
		match parse_active_local_user_id(self.services, username).await {
			| Err(e) => {
				self.services
					.admin
					.send_text(&format!("{username} is not a valid username, skipping over: {e}"))
					.await;
			},
			| Ok(user_id) => {
				if self.services.admin.user_is_admin(&user_id).await && !force {
					self.services
						.admin
						.send_text(&format!(
							"{username} is an admin and --force is not set, skipping over"
						))
						.await;

					admins.push(username);
					continue;
				}

				// don't deactivate the server service account
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
		}
	}

	let mut deactivation_count: usize = 0;

	for user_id in user_ids {
		match deactivate_user(self.services, &user_id, no_leave_rooms).await {
			| Ok(()) => {
				deactivation_count = deactivation_count.saturating_add(1);
			},
			| Err(e) => {
				self.services
					.admin
					.send_text(&format!("Failed deactivating user: {e}"))
					.await;
			},
		}
	}

	if admins.is_empty() {
		write!(self, "Deactivated {deactivation_count} accounts.")
	} else {
		write!(
			self,
			"Deactivated {deactivation_count} accounts.\nSkipped admin accounts: {}. Use \
			 --force to deactivate admin accounts",
			admins.join(", ")
		)
	}
	.await
}
