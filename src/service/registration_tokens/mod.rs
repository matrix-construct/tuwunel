mod data;

use std::{collections::HashSet, fmt::Display, sync::Arc};

use data::Data;
pub use data::{DatabaseTokenInfo, TokenExpires};
use futures::{Stream, StreamExt, pin_mut};
use tuwunel_core::{
	Err, Result, error,
	utils::{IterStream, random_string},
};

const RANDOM_TOKEN_LENGTH: usize = 16;

pub struct Service {
	db: Data,
	services: Arc<crate::services::OnceServices>,
}

/// A validated registration token which may be used to create an account.
#[derive(Debug)]
pub struct ValidToken {
	pub token: String,
	pub info: TokenInfo,
}

impl Display for ValidToken {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "`{}` --- {}", self.token, self.info)
	}
}

impl PartialEq<str> for ValidToken {
	fn eq(&self, other: &str) -> bool { self.token == other }
}

#[derive(Clone, Copy, Debug)]
pub enum TokenInfo {
	/// The static token set in the homeserver's config file, which is
	/// always valid.
	Config,
	/// A database token which has been checked to be valid.
	Database(DatabaseTokenInfo),
}

impl Display for TokenInfo {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			| Self::Config => write!(f, "Token defined in config file"),
			| Self::Database(info) => info.fmt(f),
		}
	}
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data::new(args.db),
			services: args.services.clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Create a registration token, using the caller's token or generating a
	/// random one of `length` characters (default `RANDOM_TOKEN_LENGTH`). A
	/// token that already exists is rejected.
	pub async fn create_token(
		&self,
		token: Option<&str>,
		length: Option<usize>,
		expires: TokenExpires,
	) -> Result<(String, DatabaseTokenInfo)> {
		let token = token.map(ToOwned::to_owned).unwrap_or_else(|| {
			let length = length.unwrap_or(RANDOM_TOKEN_LENGTH);

			random_string(length)
		});

		let info = self.db.save_token(&token, expires).await?;

		Ok((token, info))
	}

	/// Look up a token's stored metadata, returning `None` when it is absent.
	pub async fn get_token_info(&self, token: &str) -> Result<TokenInfo> {
		if self.get_config_tokens().await.contains(token) {
			return Ok(TokenInfo::Config);
		}

		self.db
			.get_token_info(token)
			.await
			.map(TokenInfo::Database)
	}

	/// Replace a token's expiry, preserving its use counter. Returns a `404`
	/// when the token is unknown.
	pub async fn update_token(
		&self,
		token: &str,
		expires: TokenExpires,
	) -> Result<DatabaseTokenInfo> {
		if self.get_config_tokens().await.contains(token) {
			return Err!(Request(Forbidden(
				"The token set in the config file cannot be updated"
			)));
		}

		self.db.update_token(token, expires).await
	}

	pub async fn is_enabled(&self) -> bool {
		let stream = self.iterate_tokens().await;

		pin_mut!(stream);

		stream.next().await.is_some()
	}

	pub async fn get_config_tokens(&self) -> HashSet<String> {
		let mut tokens = HashSet::new();

		if let Some(file) = &self.services.config.registration_token_file {
			match tokio::fs::read_to_string(file).await {
				| Err(e) => error!("Failed to read the registration token file: {e}"),
				| Ok(text) => tokens.extend(
					text.split_ascii_whitespace()
						.map(ToOwned::to_owned),
				),
			}
		}

		if let Some(token) = &self.services.config.registration_token {
			tokens.insert(token.to_owned());
		}

		tokens
	}

	pub async fn is_token_valid(&self, token: &str) -> Result { self.check(token, false).await }

	pub async fn try_consume(&self, token: &str) -> Result { self.check(token, true).await }

	async fn check(&self, token: &str, consume: bool) -> Result {
		if self.get_config_tokens().await.contains(token)
			|| self.db.check_token(token, consume).await
		{
			return Ok(());
		}

		Err!(Request(Forbidden("Registration token not valid")))
	}

	/// Try to revoke a valid token.
	///
	/// Note that tokens set in the config file cannot be revoked.
	pub async fn revoke_token(&self, token: &str) -> Result {
		if self.get_config_tokens().await.contains(token) {
			return Err!(Request(Forbidden(
				"The token set in the config file cannot be revoked. Edit the config file to \
				 change it."
			)));
		}

		self.db.revoke_token(token).await
	}

	/// Iterate over all valid registration tokens.
	pub async fn iterate_tokens(&self) -> impl Stream<Item = ValidToken> + Send + '_ {
		let config_tokens = self
			.get_config_tokens()
			.await
			.into_iter()
			.map(|token| ValidToken { token, info: TokenInfo::Config })
			.stream();

		let db_tokens = self
			.db
			.iterate_and_clean_tokens()
			.map(|(token, info)| ValidToken {
				token: token.to_owned(),
				info: TokenInfo::Database(info),
			});

		config_tokens.chain(db_tokens)
	}
}
