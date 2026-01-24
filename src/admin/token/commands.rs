use futures::StreamExt;
use tuwunel_core::{Result, utils};
use tuwunel_macros::admin_command;
use tuwunel_service::registration_tokens::TokenExpires;

#[admin_command]
pub(super) async fn issue(
	&self,
	max_uses: Option<u64>,
	max_age: Option<String>,
	once: bool,
) -> Result {
	let expires = TokenExpires {
		max_uses: max_uses.or_else(|| once.then_some(1)),
		max_age: max_age
			.map(|max_age| {
				let duration = utils::time::parse_duration(&max_age)?;
				utils::time::timepoint_from_now(duration)
			})
			.transpose()?,
	};

	let (token, info) = self
		.services
		.registration_tokens
		.issue_token(expires)
		.await?;

	self.write_str(&format!("New registration token issued: `{token}` - {info}",))
		.await
}

#[admin_command]
pub(super) async fn revoke(&self, token: String) -> Result {
	self.services
		.registration_tokens
		.revoke_token(&token)
		.await?;

	self.write_str("Token revoked successfully.")
		.await
}

#[admin_command]
pub(super) async fn list(&self) -> Result {
	let tokens: Vec<_> = self
		.services
		.registration_tokens
		.iterate_tokens()
		.collect()
		.await;

	self.write_str(&format!("Found {} registration tokens:\n", tokens.len()))
		.await?;

	for token in tokens {
		self.write_str(&format!("- {token}\n")).await?;
	}

	Ok(())
}
