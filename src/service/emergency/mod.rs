use std::sync::Arc;

use async_trait::async_trait;
use ruma::{
	events::{
		GlobalAccountDataEvent, GlobalAccountDataEventType, push_rules::PushRulesEventContent,
	},
	push::Ruleset,
};
use tuwunel_core::{Result, debug_warn, error, warn};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self { services: args.services.clone() }))
	}

	async fn worker(self: Arc<Self>) -> Result {
		let password = self
			.services
			.config
			.emergency_password
			.as_deref()
			.filter(|password| !password.is_empty());

		// Once the option is removed, the server user must be signed out again. That
		// only has work to do while a password from an earlier start remains.
		if password.is_none() && !self.emergency_access_remains().await {
			return Ok(());
		}

		if self.services.globals.is_read_only() {
			debug_warn!("emergency password feature ignored in read_only mode.");
			return Ok(());
		}

		if password.is_some() && self.services.config.ldap.enable {
			warn!("emergency password feature not available with LDAP enabled.");
			return Ok(());
		}

		self.set_emergency_access(password)
			.await
			.inspect_err(|e| {
				error!("Failed to update emergency access for the server user: {e}");
			})
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Whether the server user still holds a password, which only an earlier
	/// start with `emergency_password` set gives it.
	async fn emergency_access_remains(&self) -> bool {
		self.services
			.users
			.has_password(&self.services.globals.server_user)
			.await
			.unwrap_or(false)
	}

	/// Sets the emergency password and push rules for the server user account
	/// when a password is given, and removes them and signs the account out
	/// when it is not.
	async fn set_emergency_access(&self, password: Option<&str>) -> Result {
		let server_user = &self.services.globals.server_user;

		self.services
			.users
			.set_password(server_user, password)
			.await?;

		let (ruleset, pwd_set) = match password {
			| Some(_) => (Ruleset::server_default(server_user), true),
			| None => (Ruleset::new(), false),
		};

		self.services
			.account_data
			.update(
				None,
				server_user,
				GlobalAccountDataEventType::PushRules
					.to_string()
					.into(),
				&serde_json::to_value(&GlobalAccountDataEvent {
					content: PushRulesEventContent { global: ruleset },
				})
				.expect("to json value always works"),
			)
			.await?;

		if pwd_set {
			warn!(
				"The server account emergency password is set! Please unset it as soon as you \
				 finish admin account recovery! You will be logged out of the server service \
				 account when you finish."
			);
			Ok(())
		} else {
			// logs out any users still in the server service account and removes sessions
			self.services
				.users
				.deactivate_account(server_user)
				.await
		}
	}
}
