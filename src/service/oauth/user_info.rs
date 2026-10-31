use serde::{Deserialize, Serialize};
use tuwunel_core::utils::deserialize_lenient_bool;

/// Selection of userinfo response claims.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UserInfo {
	/// Unique identifier number or login username. Usually a number on most
	/// services. We consider a concatenation of the `iss` and `sub` to be a
	/// universally unique identifier for some user/identity; we index that in
	/// `oauthidpsub_oauthid`.
	///
	/// Considered for user mxid only if none of the better fields are defined.
	/// `login` alias intended for github.
	#[serde(alias = "login")]
	pub sub: String,

	/// The login username we first consider when defined.
	pub preferred_username: Option<String>,

	/// The login username considered.
	pub username: Option<String>,

	/// The login username considered if none preferred.
	pub nickname: Option<String>,

	/// Full name.
	pub name: Option<String>,

	/// First name.
	pub given_name: Option<String>,

	/// Last name.
	pub family_name: Option<String>,

	/// Email address (`email` scope).
	pub email: Option<String>,

	/// Whether the provider verified `email`, false for any claim other than a
	/// boolean or its string spelling.
	#[serde(default, deserialize_with = "deserialize_lenient_bool")]
	pub email_verified: Option<bool>,

	/// URL to pfp (github/gitlab)
	pub avatar_url: Option<String>,

	/// URL to pfp (google)
	pub picture: Option<String>,
}

impl UserInfo {
	/// The email address the provider vouches for.
	///
	/// An `email_verified` claim decides when present. Without one, only a
	/// trusted provider vouches for the address it reports.
	#[inline]
	#[must_use]
	pub fn verified_email(&self, trusted: bool) -> Option<&str> {
		self.email
			.as_deref()
			.filter(|_| self.email_verified.unwrap_or(trusted))
	}
}
