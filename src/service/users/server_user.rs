use ruma::events::StateEventType;
use tuwunel_core::{Event, Result, err, implement};

/// Rejects changes to the server identity recorded by an existing database.
///
/// The admin room must have been created by the configured user and retain its
/// membership. Without an admin alias, only an empty database or the legacy
/// identity permits startup.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn validate_server_user(&self) -> Result {
	let services = &self.services;

	let Some(room_id) = services
		.alias
		.resolve_local_alias(&services.admin.admin_alias)
		.await
		.map(Some)
		.or_else(|error| error.is_not_found().then_some(None).ok_or(error))?
	else {
		return (services.server.config.server_user_localpart == "conduit"
			|| self.is_empty().await?)
			.then_some(())
			.ok_or_else(|| {
				err!(
					"server_user_localpart can only be changed before first boot; the admin \
					 room is absent but the user database is not empty"
				)
			});
	};

	let create = services
		.state_accessor
		.room_state_get(&room_id, &StateEventType::RoomCreate, "")
		.await
		.map_err(|error| {
			err!(
				"Cannot validate server_user_localpart against the admin room create event: \
				 {error}"
			)
		})?;

	let server_user = services.globals.server_user.as_ref();

	if create.sender() != server_user
		|| !services
			.state_cache
			.is_joined(server_user, &room_id)
			.await
	{
		return Err(err!(
			"server_user_localpart does not match the established admin room identity; changing \
			 it after first boot is unsupported"
		));
	}

	Ok(())
}
