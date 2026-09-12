use tuwunel_core::{
	Err, Result,
	config::ServerUserLocalpart,
	err, implement,
	utils::{BoolExt, result::NotFound},
	warn,
};
use tuwunel_database::Deserialized;

/// Key under which the server user's localpart is stamped at first boot.
///
/// Every later boot compares its configured localpart against the stamp.
pub const SERVER_USER_KEY: &[u8] = b"server_user_localpart";

/// Rejects changes to the server identity recorded by an existing database.
///
/// The localpart is stamped on the first boot and compared exactly afterwards.
/// A database from before the stamp is backfilled from its admin room.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn validate_server_user(&self) -> Result {
	let config = &self.services.server.config;
	let configured = config.server_user_localpart.as_str();

	let established = self.services.db["global"]
		.get(SERVER_USER_KEY)
		.await
		.deserialized::<ServerUserLocalpart>()
		.optional()?;

	match established {
		| None => self.backfill_server_user(configured).await,
		| Some(established) if established == configured => Ok(()),
		| Some(established) => Err!(Database(
			"server_user_localpart is {configured} but this database established {established}; \
			 changing it after first boot is unsupported"
		)),
	}
}

/// Stamps the identity on a database that pre-dates SERVER_USER_KEY.
///
/// Membership of the admin room establishes the identity, or an empty user
/// table when there is no admin room; room creation does not, since a rebuilt
/// admin room has a human creator. The legacy identity otherwise boots
/// unstamped and any other is refused. A read-only database is validated but
/// never stamped.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn backfill_server_user(&self, configured: &str) -> Result {
	let established = match self.admin_room_joined().await? {
		| Some(joined) => joined,
		| None => self.is_empty().await?,
	};

	let legacy = configured == "conduit";

	if !established {
		return BoolExt::ok_or_else(legacy, || {
			err!(Database(
				"server_user_localpart is {configured} but neither the admin room nor an empty \
				 user table establishes that identity; changing it after first boot is \
				 unsupported"
			))
		});
	}

	if !self.services.globals.is_read_only() {
		self.services.db["global"].insert(SERVER_USER_KEY, configured);
	}

	Ok(())
}

/// Reports whether the server user is joined to the admin room, or nothing
/// without an admin alias.
///
/// An unjoined server user also means admin commands cannot work.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn admin_room_joined(&self) -> Result<Option<bool>> {
	let services = &self.services;

	let Some(room_id) = services
		.alias
		.resolve_local_alias(&services.admin.admin_alias)
		.await
		.optional()?
	else {
		return Ok(None);
	};

	let server_user = services.globals.server_user.as_ref();
	let joined = services
		.state_cache
		.is_joined(server_user, &room_id)
		.await;

	if !joined {
		warn!(
			%room_id,
			%server_user,
			"The admin room does not include the server user; admin commands are unavailable \
			 until it does"
		);
	}

	Ok(Some(joined))
}
