mod data;

use std::{ops::Range, sync::Arc};

pub use data::Data;
use ruma::{OwnedUserId, RoomAliasId, ServerName, UserId};
use tuwunel_core::{
	Result, Server, err,
	utils::{Secret, resolve_secret},
};

use crate::service;

pub struct Service {
	pub db: Data,
	server: Arc<Server>,

	pub server_user: OwnedUserId,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let db = Data::new(args);

		let server_user =
			server_user(&args.server.config.server_user_localpart, &args.server.name)?;

		Ok(Arc::new(Self {
			db,
			server: args.server.clone(),
			server_user,
		}))
	}

	fn name(&self) -> &str { service::make_name(std::module_path!()) }
}

/// Resolves the configured localpart to this server's administrative user.
///
/// Full user IDs are rejected so the configuration cannot select an identity
/// belonging to another server. Invalid input names the configuration setting.
fn server_user(localpart: &str, server_name: &ServerName) -> Result<OwnedUserId> {
	UserId::parse_with_server_name(localpart, server_name)
		.map_err(|e| err!("Invalid server_user_localpart configuration: {e}"))
		.and_then(|user| {
			user.localpart()
				.eq(localpart)
				.then_some(user)
				.ok_or_else(|| {
					err!("server_user_localpart must be a localpart, not a full user ID")
				})
		})
}

impl Service {
	#[tracing::instrument(
		level = "trace",
		skip_all,
		ret,
		fields(pending = ?self.pending_count()),
	)]
	pub async fn wait_pending(&self) -> Result<u64> { self.db.wait_pending().await }

	#[tracing::instrument(
		level = "trace",
		skip_all,
		ret,
		fields(pending = ?self.pending_count()),
	)]
	pub async fn wait_count(&self, count: &u64) -> Result<u64> { self.db.wait_count(count).await }

	#[tracing::instrument(
		level = "debug",
		skip_all,
		fields(pending = ?self.pending_count()),
	)]
	#[must_use]
	pub fn next_count(&self) -> data::Permit { self.db.next_count() }

	#[must_use]
	pub fn current_count(&self) -> u64 { self.db.current_count() }

	#[must_use]
	pub fn pending_count(&self) -> Range<u64> { self.db.pending_count() }

	#[inline]
	#[must_use]
	pub fn server_name(&self) -> &ServerName { self.server.name.as_ref() }

	/// checks if `user_id` is local to us via server_name comparison
	#[inline]
	#[must_use]
	pub fn user_is_local(&self, user_id: &UserId) -> bool {
		self.server_is_ours(user_id.server_name())
	}

	#[inline]
	#[must_use]
	pub fn alias_is_local(&self, alias: &RoomAliasId) -> bool {
		self.server_is_ours(alias.server_name())
	}

	#[inline]
	#[must_use]
	pub fn server_is_ours(&self, server_name: &ServerName) -> bool {
		server_name == self.server_name()
	}

	#[inline]
	#[must_use]
	pub fn is_read_only(&self) -> bool { self.db.db.is_read_only() }

	/// Reads `turn_secret_file` on every call, so a rotated secret takes effect
	/// without a restart.
	#[must_use]
	pub fn turn_secret(&self) -> Option<Secret> {
		let config = &self.server.config;

		resolve_secret(
			config.turn_secret_file.as_deref(),
			config.turn_secret.as_deref(),
			"TURN secret",
		)
	}

	pub fn init_rustls_provider(&self) -> Result {
		if rustls::crypto::CryptoProvider::get_default().is_none() {
			rustls::crypto::aws_lc_rs::default_provider()
				.install_default()
				.map_err(|_provider| {
					err!(error!("Error initialising aws_lc_rs rustls crypto backend"))
				})
		} else {
			Ok(())
		}
	}
}

#[cfg(test)]
mod tests {
	use ruma::server_name;

	use super::server_user;

	#[test]
	fn configured_server_user_must_be_valid() {
		let server_name = server_name!("example.org");

		assert_eq!(server_user("conduit", server_name).unwrap(), "@conduit:example.org");
		assert_eq!(server_user("_server", server_name).unwrap(), "@_server:example.org");
		server_user("bad:localpart", server_name).unwrap_err();
		server_user("@conduit:example.org", server_name).unwrap_err();
		server_user("@conduit:elsewhere.org", server_name).unwrap_err();
	}
}
