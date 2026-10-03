use ruma::{
	OwnedUserId,
	api::client::session::login::v3::{Request, Token},
};
use tuwunel_core::{Err, Result};
use tuwunel_service::Services;

use crate::{Ruma, router::auth::jwt::validate_user};

pub(super) async fn handle_login(
	services: &Services,
	_body: &Ruma<Request>,
	info: &Token,
) -> Result<OwnedUserId> {
	let user_id = validate_user(services, &info.token)?;

	if !services.users.exists(&user_id).await {
		let config = &services.config.jwt;

		if !config.register_user {
			return Err!(Request(NotFound("User {user_id} is not registered on this server.")));
		}

		services
			.users
			.create(&user_id, Some("*"), Some("jwt"))
			.await?;
	}

	Ok(user_id)
}
