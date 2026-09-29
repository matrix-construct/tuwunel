use tuwunel_core::{Err, Result};

use super::deactivate_user;
use crate::{admin_command, utils::parse_existing_local_user_id};

#[admin_command]
pub(super) async fn deactivate(&self, no_leave_rooms: bool, user_id: String) -> Result {
	let user_id = parse_existing_local_user_id(self.services, &user_id).await?;

	// don't deactivate the server service account
	if user_id == self.services.globals.server_user {
		return Err!("Not allowed to deactivate the server service account.",);
	}

	deactivate_user(self.services, &user_id, no_leave_rooms).await?;

	write!(self, "User {user_id} has been deactivated").await
}
