use std::{sync::Arc, time::SystemTime};

use futures::Stream;
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Result,
	utils::{
		self,
		stream::{ReadyExt, TryIgnore},
	},
};
use tuwunel_database::{Database, Deserialized, Json, Map};

pub(super) struct Data {
	registrationtoken_info: Arc<Map>,
}

/// Metadata of a registration token.
#[derive(Debug, Serialize, Deserialize)]
pub struct DatabaseTokenInfo {
	/// The number of times this token has been used to create an account.
	pub uses: u64,
	/// When this token will expire, if it expires.
	pub expires: TokenExpires,
}

impl DatabaseTokenInfo {
	pub(super) fn new(expires: TokenExpires) -> Self { Self { uses: 0, expires } }

	/// Determine whether this token info represents a valid token, i.e. one
	/// that has not expired according to its [`Self::expires`] property. If
	/// [`Self::expires`] is [`None`], this function will always return `true`.
	#[must_use]
	pub fn is_valid(&self) -> bool {
		if let Some(max_uses) = self.expires.max_uses
			&& max_uses >= self.uses
		{
			return false;
		}

		if let Some(max_age) = self.expires.max_age {
			let now = SystemTime::now();

			if now > max_age {
				return false;
			}
		}

		true
	}
}

impl std::fmt::Display for DatabaseTokenInfo {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "Token used {} times. {}", self.uses, self.expires)?;

		Ok(())
	}
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenExpires {
	pub max_uses: Option<u64>,
	pub max_age: Option<SystemTime>,
}

impl std::fmt::Display for TokenExpires {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let mut msgs = vec![];

		if let Some(max_uses) = self.max_uses {
			msgs.push(format!("after {max_uses} uses"));
		}

		if let Some(max_age) = self.max_age {
			let now = SystemTime::now();
			let expires_at = utils::time::format(max_age, "%F %T");

			match max_age.duration_since(now) {
				| Ok(duration) => {
					let expires_in = utils::time::pretty(duration);
					msgs.push(format!("in {expires_in} ({expires_at})"));
				},
				| Err(_) => {
					write!(f, "Expired at {expires_at}")?;
					return Ok(());
				},
			}
		}

		if !msgs.is_empty() {
			write!(f, "Expires {}.", msgs.join(" or "))?;
		} else {
			write!(f, "Never expires.")?;
		}

		Ok(())
	}
}

impl Data {
	pub(super) fn new(db: &Arc<Database>) -> Self {
		Self {
			registrationtoken_info: db["registrationtoken_info"].clone(),
		}
	}

	/// Associate a registration token with its metadata in the database.
	pub(super) async fn save_token(
		&self,
		token: &str,
		expires: TokenExpires,
	) -> Result<DatabaseTokenInfo> {
		if self
			.registrationtoken_info
			.exists(token)
			.await
			.is_err()
		{
			let info = DatabaseTokenInfo::new(expires);

			self.registrationtoken_info
				.raw_put(token, Json(&info));

			Ok(info)
		} else {
			Err!(Request(InvalidParam("Registration token already exists")))
		}
	}

	/// Delete a registration token.
	pub(super) async fn revoke_token(&self, token: &str) -> Result {
		if self
			.registrationtoken_info
			.exists(token)
			.await
			.is_ok()
		{
			self.registrationtoken_info.remove(token);

			Ok(())
		} else {
			Err!(Request(NotFound("Registration token not found")))
		}
	}

	/// Look up a registration token's metadata.
	pub(super) async fn check_token(&self, token: &str, consume: bool) -> bool {
		let info = self
			.registrationtoken_info
			.get(token)
			.await
			.deserialized::<DatabaseTokenInfo>()
			.ok();

		info.map(|mut info| {
			if !info.is_valid() {
				self.registrationtoken_info.remove(token);
				return false;
			}

			if consume {
				info.uses = info.uses.saturating_add(1);

				if info.is_valid() {
					self.registrationtoken_info
						.raw_put(token, Json(info));
				} else {
					self.registrationtoken_info.remove(token);
				}
			}

			true
		})
		.unwrap_or(false)
	}

	/// Iterate over all valid tokens and delete expired ones.
	pub(super) fn iterate_and_clean_tokens(
		&self,
	) -> impl Stream<Item = (&str, DatabaseTokenInfo)> + Send + '_ {
		self.registrationtoken_info
			.stream()
			.ignore_err()
			.ready_filter_map(|(token, info): (&str, DatabaseTokenInfo)| {
				if info.is_valid() {
					Some((token, info))
				} else {
					self.registrationtoken_info.remove(token);
					None
				}
			})
	}
}
