use tuwunel_core::{Err, Result, utils};

use super::AUTO_GEN_PASSWORD_LENGTH;
use crate::{admin_command, utils::parse_existing_local_user_id};

#[admin_command]
pub(super) async fn reset_password(&self, username: String, password: Option<String>) -> Result {
	let user_id = parse_existing_local_user_id(self.services, &username).await?;

	if user_id == self.services.globals.server_user {
		return Err!(
			"Not allowed to set the password for the server account. Please use the emergency \
			 password config option.",
		);
	}

	let new_password = password.unwrap_or_else(|| utils::random_string(AUTO_GEN_PASSWORD_LENGTH));

	match self
		.services
		.users
		.set_password(&user_id, Some(new_password.as_str()))
		.await
	{
		| Err(e) => return Err!("Couldn't reset the password for user {user_id}: {e}"),
		| Ok(()) => {
			write!(self, "Successfully reset the password for user {user_id}: `{new_password}`")
		},
	}
	.await
}
