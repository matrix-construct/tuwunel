use tuwunel_core::{Err, Result};

use crate::{admin_command, utils::parse_local_user_id};

#[admin_command]
pub(super) async fn revoke_admin(&self, user_id: String) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;

	if self
		.sender
		.is_some_and(|sender| sender == user_id)
	{
		return Err!("You may not revoke your own admin privileges.");
	}

	self.services.admin.revoke_admin(&user_id).await?;

	write!(self, "{user_id} is no longer an admin.").await
}
