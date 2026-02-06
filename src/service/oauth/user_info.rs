use serde::{Deserialize, Serialize};

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

	/// URL to pfp (github/gitlab)
	pub avatar_url: Option<String>,

	/// URL to pfp (google)
	pub picture: Option<String>,
}
