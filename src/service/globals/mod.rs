//! Global server identity and sequence state.
//!
//! The service exposes the local server identity, shared monotonic counter, and process-wide
//! security settings. Counter permits separate dispatched values from values whose writes are
//! safe for readers to observe.

mod data;

use std::{ops::Range, sync::Arc};

/// Persistent storage and retirement tracking for the global sequence counter.
///
/// Migration and lifecycle code use this storage directly to access the database version. Request
/// paths normally use [`Service`] instead.
pub use data::Data;
use ruma::{OwnedUserId, RoomAliasId, ServerName, UserId};
use tuwunel_core::{
	Result, Server, err,
	utils::{Secret, resolve_secret},
};

use crate::service;

/// Provides process-wide server identity, secrets, and monotonic sequence numbers.
///
/// Sequence numbers are persisted when dispatched and become readable only after their permits
/// retire. The service also centralizes locality checks against the configured server name.
pub struct Service {
	/// Persistent global counter and database version storage.
	pub db: Data,
	server: Arc<Server>,

	/// Local user ID reserved for homeserver administration.
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
	/// Waits until every sequence number dispatched at call time has retired.
	///
	/// The dispatched frontier is snapshotted before waiting. The returned value is the retirement
	/// frontier that reached the sampled value.
	#[tracing::instrument(
		level = "trace",
		skip_all,
		ret,
		fields(pending = ?self.pending_count()),
	)]
	pub async fn wait_pending(&self) -> Result<u64> { self.db.wait_pending().await }

	/// Waits for the retirement frontier to reach a sequence number.
	///
	/// Completion means all writes through `count` are globally visible to readers. The returned
	/// value may be greater when later writes retired while waiting.
	#[tracing::instrument(
		level = "trace",
		skip_all,
		ret,
		fields(pending = ?self.pending_count()),
	)]
	pub async fn wait_count(&self, count: &u64) -> Result<u64> { self.db.wait_count(count).await }

	/// Dispatches the next persistent sequence number.
	///
	/// The returned permit dereferences to the allocated number. Dropping it retires the associated
	/// write and may advance the reader-visible frontier.
	///
	/// # Panics
	///
	/// Panics when the counter is exhausted or the dispatched value cannot be recorded.
	#[tracing::instrument(
		level = "debug",
		skip_all,
		fields(pending = ?self.pending_count()),
	)]
	#[must_use]
	pub fn next_count(&self) -> data::Permit { self.db.next_count() }

	/// Returns the highest sequence number whose writes have retired.
	///
	/// Readers can safely use this value as an upper bound for globally visible writes.
	#[must_use]
	pub fn current_count(&self) -> u64 { self.db.current_count() }

	/// Returns a snapshot of the retired and dispatched counter frontiers.
	///
	/// The range start is the highest reader-visible number and the range end is the latest number
	/// dispatched to a writer.
	#[must_use]
	pub fn pending_count(&self) -> Range<u64> { self.db.pending_count() }

	/// Returns the configured local server name.
	///
	/// The returned name is borrowed from the server-wide configuration.
	#[inline]
	#[must_use]
	pub fn server_name(&self) -> &ServerName { self.server.name.as_ref() }

	/// Reports whether a user ID belongs to the local server.
	///
	/// Locality is determined solely by comparing the ID's server name with [`Self::server_name`].
	#[inline]
	#[must_use]
	pub fn user_is_local(&self, user_id: &UserId) -> bool {
		self.server_is_ours(user_id.server_name())
	}

	/// Reports whether a room alias belongs to the local server.
	///
	/// Locality is determined solely by comparing the alias server name with
	/// [`Self::server_name`].
	#[inline]
	#[must_use]
	pub fn alias_is_local(&self, alias: &RoomAliasId) -> bool {
		self.server_is_ours(alias.server_name())
	}

	/// Reports whether a server name identifies this homeserver.
	///
	/// The comparison uses the configured local server name without resolving aliases or delegated
	/// hosting.
	#[inline]
	#[must_use]
	pub fn server_is_ours(&self, server_name: &ServerName) -> bool {
		server_name == self.server_name()
			|| self
				.server
				.config
				.alternate_server_names
				.iter()
				.any(|s| s == server_name)
	}

	/// Reports whether the database is open in read-only mode.
	///
	/// The value is delegated to the active database engine.
	#[inline]
	#[must_use]
	pub fn is_read_only(&self) -> bool { self.db.db.is_read_only() }

	/// Resolves the secret used to authenticate TURN credentials.
	///
	/// The configured secret file is read and trimmed on every call, allowing rotation without a
	/// restart. A successfully read file, including an empty file, takes precedence; read failures
	/// fall back to the inline secret.
	#[must_use]
	pub fn turn_secret(&self) -> Option<Secret> {
		let config = &self.server.config;

		resolve_secret(
			config.turn_secret_file.as_deref(),
			config.turn_secret.as_deref(),
			"TURN secret",
		)
	}

	/// Installs the default rustls cryptography provider when none exists.
	///
	/// Existing process-wide providers are preserved. A failure to install the AWS-LC provider is
	/// returned to the caller.
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
