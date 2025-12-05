use std::fmt::Write;

use futures::FutureExt;
use ruma::{UserId, events::GlobalAccountDataEventType, push};
use tuwunel_core::{Err, Result, error, implement, info, is_equal_to, warn};

use crate::appservice::RegistrationInfo;

/// Fully register a local user
///
/// Returns a device id and access token for the registered user
#[implement(super::Service)]
pub async fn full_register(
	&self,
	user_id: &UserId,
	password: Option<&str>,
	origin: Option<&str>,
	appservice_info: Option<&RegistrationInfo>,
	is_guest: bool,
) -> Result {
	if !self.services.globals.user_is_local(user_id) {
		return Err!("Cannot register remote user");
	}

	if self.services.users.exists(user_id).await {
		return Err!(Request(UserInUse("User ID is not available.")));
	}

	// Create user
	self.services
		.users
		.create(user_id, password, origin)
		.await?;

	// Default to pretty displayname
	let mut displayname = user_id.localpart().to_owned();

	// If `new_user_displayname_suffix` is set, registration will push whatever
	// content is set to the user's display name with a space before it
	if !self
		.services
		.config
		.new_user_displayname_suffix
		.is_empty()
		&& appservice_info.is_none()
	{
		write!(
			displayname,
			" {}",
			self.services
				.server
				.config
				.new_user_displayname_suffix
		)?;
	}

	self.services
		.users
		.set_displayname(user_id, Some(displayname.clone()));

	// Initial account data
	self.services
		.account_data
		.update(
			None,
			user_id,
			GlobalAccountDataEventType::PushRules
				.to_string()
				.into(),
			&serde_json::to_value(ruma::events::push_rules::PushRulesEvent {
				content: ruma::events::push_rules::PushRulesEventContent {
					global: push::Ruleset::server_default(user_id),
				},
			})?,
		)
		.await?;

	// If this is the first real user, grant them admin privileges except for guest
	// users
	// Note: the server user is generated first
	if !is_guest
		&& self.services.config.grant_admin_to_first_user
		&& let Ok(admin_room) = self.services.admin.get_admin_room().await
		&& self
			.services
			.state_cache
			.room_joined_count(&admin_room)
			.await
			.is_ok_and(is_equal_to!(1))
	{
		self.services
			.admin
			.make_user_admin(user_id)
			.boxed()
			.await?;
		warn!("Granting {user_id} admin privileges as the first user");
	}

	if appservice_info.is_none()
		&& (self.services.config.allow_guests_auto_join_rooms || !is_guest)
	{
		for room in &self.services.server.config.auto_join_rooms {
			let Ok(room_id) = self.services.alias.maybe_resolve(room).await else {
				error!(
					"Failed to resolve room alias to room ID when attempting to auto join \
					 {room}, skipping"
				);
				continue;
			};

			if !self
				.services
				.state_cache
				.server_in_room(self.services.globals.server_name(), &room_id)
				.await
			{
				warn!(
					"Skipping room {room} to automatically join as we have never joined before."
				);
				continue;
			}

			let state_lock = self.services.state.mutex.lock(&room_id).await;

			match self
				.services
				.membership
				.join(
					user_id,
					&room_id,
					Some(room),
					Some("Automatically joining this room upon registration".to_owned()),
					&[],
					false,
					&state_lock,
				)
				.boxed()
				.await
			{
				| Err(e) => {
					// don't return this error so we don't fail registrations
					error!("Failed to automatically join room {room} for user {user_id}: {e}");
				},
				| _ => {
					info!("Automatically joined room {room} for user {user_id}");
				},
			}

			drop(state_lock);
		}
	}

	Ok(())
}
