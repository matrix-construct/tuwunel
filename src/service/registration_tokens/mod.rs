mod data;

use std::{collections::HashSet, sync::Arc};

use data::Data;
pub use data::{DatabaseTokenInfo, TokenExpires};
use futures::{Stream, StreamExt, pin_mut};
use tuwunel_core::{
	Err, Result, error,
	utils::{self, IterStream},
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
	pub source: ValidTokenSource,
}

impl std::fmt::Display for ValidToken {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "`{}` --- {}", self.token, &self.source)
	}
}

impl PartialEq<str> for ValidToken {
	fn eq(&self, other: &str) -> bool { self.token == other }
}

/// The source of a valid database token.
#[derive(Debug)]
pub enum ValidTokenSource {
	/// The static token set in the homeserver's config file, which is
	/// always valid.
	ConfigFile,
	/// A database token which has been checked to be valid.
	Database(DatabaseTokenInfo),
}

impl std::fmt::Display for ValidTokenSource {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			| Self::ConfigFile => write!(f, "Token defined in config."),
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
	/// Issue a new registration token and save it in the database.
	pub async fn issue_token(
		&self,
		expires: TokenExpires,
	) -> Result<(String, DatabaseTokenInfo)> {
		let token = utils::random_string(RANDOM_TOKEN_LENGTH);

		let info = self.db.save_token(&token, expires).await?;

		Ok((token, info))
	}

	pub async fn is_enabled(&self) -> bool {
		let stream = self.iterate_tokens();

		pin_mut!(stream);

		stream.next().await.is_some()
	}

	pub fn get_config_tokens(&self) -> HashSet<String> {
		let mut tokens = HashSet::new();
		if let Some(file) = &self
			.services
			.server
			.config
			.registration_token_file
			.as_ref()
		{
			match std::fs::read_to_string(file) {
				| Err(e) => error!("Failed to read the registration token file: {e}"),
				| Ok(text) => {
					text.split_ascii_whitespace().for_each(|token| {
						tokens.insert(token.to_owned());
					});
				},
			}
		}

		if let Some(token) = &self.services.server.config.registration_token {
			tokens.insert(token.to_owned());
		}

		tokens
	}

	pub async fn is_token_valid(&self, token: &str) -> Result { self.check(token, false).await }

	pub async fn try_consume(&self, token: &str) -> Result { self.check(token, true).await }

	async fn check(&self, token: &str, consume: bool) -> Result {
		if self.get_config_tokens().contains(token) || self.db.check_token(token, consume).await {
			return Ok(());
		}

		Err!(Request(Forbidden("Registration token not valid")))
	}

	/// Try to revoke a valid token.
	///
	/// Note that tokens set in the config file cannot be revoked.
	pub async fn revoke_token(&self, token: &str) -> Result {
		if self.get_config_tokens().contains(token) {
			return Err!(
				"The token set in the config file cannot be revoked. Edit the config file to \
				 change it."
			);
		}

		self.db.revoke_token(token).await
	}

	/// Iterate over all valid registration tokens.
	pub fn iterate_tokens(&self) -> impl Stream<Item = ValidToken> + Send + '_ {
		let config_tokens = self
			.get_config_tokens()
			.into_iter()
			.map(|token| ValidToken {
				token,
				source: ValidTokenSource::ConfigFile,
			})
			.stream();

		let db_tokens = self
			.db
			.iterate_and_clean_tokens()
			.map(|(token, info)| ValidToken {
				token: token.to_owned(),
				source: ValidTokenSource::Database(info),
			});

		config_tokens.chain(db_tokens)
	}
}
