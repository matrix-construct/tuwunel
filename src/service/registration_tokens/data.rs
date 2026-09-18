//! Persistent registration token metadata.
//!
//! Database tokens retain a stored successful-use count and optional expiration limits. Validation
//! removes records when the stored count reaches its threshold or the deadline passes.

use std::{sync::Arc, time::SystemTime};

use futures::Stream;
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Result, err,
	utils::{
		self,
		stream::{ReadyExt, TryIgnore},
	},
};
use tuwunel_database::{Database, Deserialized, Json, Map};

/// Provides database operations for registration tokens.
///
/// Each token maps directly to its serialized [`DatabaseTokenInfo`]. Expired or exhausted tokens
/// are removed by validation and iteration paths.
pub(super) struct Data {
	registrationtoken_info: Arc<Map>,
}

/// Persistent usage and expiration metadata for a registration token.
///
/// Each successful consumption writes an incremented counter snapshot. Concurrent consumers are
/// not serialized and can share a prior count. Expiration limits are evaluated when the token is
/// checked or listed.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DatabaseTokenInfo {
	/// The stored number of successful token consumptions.
	pub uses: u64,

	/// When this token will expire, if it expires.
	pub expires: TokenExpires,
}

impl DatabaseTokenInfo {
	/// Creates unused metadata with the supplied expiration policy.
	///
	/// New tokens always begin with a zero use count.
	pub(super) fn new(expires: TokenExpires) -> Self { Self { uses: 0, expires } }

	/// Reports whether the token remains within every expiration limit.
	///
	/// A token is invalid after using its full allowance or passing its absolute deadline. Metadata
	/// without either limit remains valid indefinitely.
	#[must_use]
	pub fn is_valid(&self) -> bool {
		if let Some(max_uses) = self.expires.max_uses
			&& self.uses >= max_uses
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

/// Optional limits governing a database-backed registration token.
///
/// Either limit can be absent independently. When both are absent, the token does not expire.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TokenExpires {
	/// Stored use-count threshold at which the token becomes invalid.
	pub max_uses: Option<u64>,

	/// Absolute time after which the token is invalid.
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
	/// Opens the registration token metadata map.
	///
	/// The returned handle shares the caller's database instance.
	pub(super) fn new(db: &Arc<Database>) -> Self {
		Self {
			registrationtoken_info: db["registrationtoken_info"].clone(),
		}
	}

	/// Stores a new registration token and its expiration policy.
	///
	/// The token begins with zero uses. A token already present in the database is rejected.
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

	/// Deletes a registration token from persistent storage.
	///
	/// Unknown tokens produce a not-found request error.
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

	/// Checks a stored token and optionally consumes one use.
	///
	/// Invalid tokens are deleted. A successful consumption writes an incremented count or removes the
	/// token when the updated count reaches its threshold. Concurrent checks are not serialized.
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

	/// Loads a database token's stored metadata.
	///
	/// The metadata is returned without checking expiration. Missing or undecodable rows produce a
	/// not-found request error.
	pub(super) async fn get_token_info(&self, token: &str) -> Result<DatabaseTokenInfo> {
		self.registrationtoken_info
			.get(token)
			.await
			.deserialized()
			.map_err(|_| err!(Request(NotFound("Registration token not found"))))
	}

	/// Replaces a token's expiration policy while preserving its use count.
	///
	/// The token must already exist. The updated metadata is persisted and returned.
	pub(super) async fn update_token(
		&self,
		token: &str,
		expires: TokenExpires,
	) -> Result<DatabaseTokenInfo> {
		let current = self.get_token_info(token).await?;

		let info = DatabaseTokenInfo { uses: current.uses, expires };

		self.registrationtoken_info
			.raw_put(token, Json(&info));

		Ok(info)
	}

	/// Streams every valid database token and removes invalid entries.
	///
	/// Unreadable rows are skipped before validation. Expired or exhausted tokens are deleted as the
	/// stream is consumed.
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
