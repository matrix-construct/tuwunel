mod data;

use std::{ops::Range, sync::Arc};

use data::Data;
use ruma::{OwnedUserId, RoomAliasId, ServerName, UserId};
use tuwunel_core::{Result, Server, error};

use crate::service;

pub struct Service {
	pub db: Data,
	server: Arc<Server>,

	pub server_user: OwnedUserId,
	pub turn_secret: String,
	pub registration_token: Option<String>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let db = Data::new(args);
		let config = &args.server.config;

		let turn_secret = config.turn_secret_file.as_ref().map_or_else(
			|| config.turn_secret.clone(),
			|path| {
				std::fs::read_to_string(path).unwrap_or_else(|e| {
					error!("Failed to read the TURN secret file: {e}");

					config.turn_secret.clone()
				})
			},
		);

		let registration_token = config
			.registration_token_file
			.as_ref()
			.map_or_else(
				|| config.registration_token.clone(),
				|path| {
					let Ok(token) = std::fs::read_to_string(path).inspect_err(|e| {
						error!("Failed to read the registration token file: {e}");
					}) else {
						return config.registration_token.clone();
					};

					Some(token)
				},
			);

		Ok(Arc::new(Self {
			db,
			server: args.server.clone(),
			server_user: UserId::parse_with_server_name(
				&config.server_user,
				&args.server.name,
			)
			.expect("server user is valid"),
			turn_secret,
			registration_token,
		}))
	}

	fn name(&self) -> &str { service::make_name(std::module_path!()) }
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
}
