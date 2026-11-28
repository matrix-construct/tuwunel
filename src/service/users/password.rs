use std::net::IpAddr;

use futures::TryFutureExt;
use ruma::{OwnedUserId, UserId};
use tuwunel_core::{
	Result, debug_error, err, error, implement,
	utils::{hash::verify_password, result::NotFound},
};

use super::is_password_hash;

/// Checks a password sign-in to a local account.
///
/// Tries the account `user_id` names, then the one its lowercased localpart
/// names. No such account, a wrong password, an account with no password to
/// check and one that signs in through a provider all get the same refusal,
/// and each keeps the failed-attempt token. Returns the account and the hash
/// its password verified against.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all, fields(%user_id))]
pub async fn password_login(
	&self,
	user_id: &UserId,
	password: &str,
	client: IpAddr,
) -> Result<(OwnedUserId, String)> {
	// Before any account lookup, so a refusal reads alike for real and unknown accounts.
	let reservation = self
		.services
		.login_ratelimit
		.reserve_login_attempt(user_id, client)?;

	let invalid = || err!(Request(Forbidden("Invalid username or password.")));
	let lowercased = async |error| -> Result<_> {
		let localpart = user_id.localpart().to_lowercase();

		if localpart == user_id.localpart() {
			return Err(error);
		}

		// Whole, since a historical localpart may itself start with `@`.
		let lowercased = UserId::parse(format!("@{localpart}:{}", user_id.server_name()))?;
		let hash = self.password_hash(&lowercased).await?;

		Ok((lowercased, hash))
	};

	let (user_id, hash) = self
		.password_hash(user_id)
		.map_ok(|hash| (user_id.to_owned(), hash))
		.or_else(lowercased)
		.map_err(|_| invalid())
		.await?;

	// Other origins sign in through their provider; a legacy account has none.
	let checkable = is_password_hash(&hash)
		&& self
			.origin(&user_id)
			.await
			.present()
			.inspect_err(
				|error| error!(%user_id, %error, "Refusing a password sign-in on an unreadable origin"),
			)
			.is_ok_and(|origin| origin.is_none_or(|origin| origin == "password"));

	if !checkable {
		return Err(invalid());
	}

	verify_password(password, &hash)
		.inspect_err(|error| debug_error!(%error, "Password verification failed"))
		.map_err(|_| invalid())?;

	self.services
		.login_ratelimit
		.record_login(reservation)?;

	Ok((user_id, hash))
}
