use std::sync::Arc;

use async_trait::async_trait;
use ruma::{
	events::{
		GlobalAccountDataEvent, GlobalAccountDataEventType, push_rules::PushRulesEventContent,
	},
	push::Ruleset,
};
use serde_json::to_value;
use tuwunel_core::{Result, debug_warn, error, implement, warn};

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
			.inspect_err(|e| error!(%e, "Failed to update emergency access for the server user"))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Whether the server user still holds a password.
///
/// Only an earlier start with `emergency_password` set gives it one.
#[implement(Service)]
async fn emergency_access_remains(&self) -> bool {
	self.services
		.users
		.has_password(&self.services.globals.server_user)
		.await
		.unwrap_or(false)
}

/// Sets or removes the server user's emergency access.
///
/// A given password is set along with the default push rules. Without one, the
/// push rules are cleared and the account is signed out and deactivated.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn set_emergency_access(&self, password: Option<&str>) -> Result {
	let server_user = &self.services.globals.server_user;

	// The password marks the account for cleanup on a later start, so on
	// removal only `deactivate_account`, the last step, clears it.
	if password.is_some() {
		self.services
			.users
			.set_password(server_user, password)
			.await?;
	}

	let ruleset = password.map_or_else(Ruleset::new, |_| Ruleset::server_default(server_user));
	let event_type = GlobalAccountDataEventType::PushRules
		.to_string()
		.into();

	let content = to_value(&GlobalAccountDataEvent {
		content: PushRulesEventContent { global: ruleset },
	})
	.expect("to json value always works");

	self.services
		.account_data
		.update(None, server_user, event_type, &content)
		.await?;

	if password.is_some() {
		warn!(
			"The server account emergency password is set! Please unset it as soon as you \
			 finish admin account recovery! You will be logged out of the server service \
			 account when you finish."
		);

		return Ok(());
	}

	// Before the password is cleared, so an interrupted revocation retries next start.
	self.services
		.oauth
		.revoke_user_tokens(server_user)
		.await;

	// Never refused: the last-admin check does not count the server user.
	self.services
		.users
		.deactivate_account(server_user)
		.await
}
