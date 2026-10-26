use tuwunel_core::{Err, Result, utils};
use tuwunel_service::users::Register;

use super::AUTO_GEN_PASSWORD_LENGTH;
use crate::{admin_command, utils::parse_local_user_id};

#[admin_command]
pub(super) async fn create_user(&self, username: String, password: Option<String>) -> Result {
	// Validate user id
	let user_id = parse_local_user_id(self.services, &username)?;

	if let Err(e) = user_id.validate_strict()
		&& self
			.services
			.config
			.authentication
			.emergency_password
			.is_none()
	{
		return Err!("Username {user_id} contains disallowed characters or spaces: {e}");
	}

	if self.services.users.exists(&user_id).await {
		return Err!("User {user_id} already exists");
	}

	let password = password.unwrap_or_else(|| utils::random_string(AUTO_GEN_PASSWORD_LENGTH));

	self.services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some(&password),
			grant_first_user_admin: true,
			..Default::default()
		})
		.await?;

	write!(self, "Created user with user_id: {user_id} and password: `{password}`").await
}
