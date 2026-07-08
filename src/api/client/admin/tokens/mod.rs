//! Synapse admin API: registration-token endpoints.

mod create;
mod delete;
mod get;
mod list;
mod update;

use std::time::Duration;

use ruma::{MilliSecondsSinceUnixEpoch, UInt, uint};
use synapse_admin_api::registration_tokens::RegistrationToken;
use tuwunel_core::{Result, utils::time::timepoint_from_epoch};
use tuwunel_service::registration_tokens::{
	DatabaseTokenInfo, TokenExpires, ValidToken, ValidTokenSource,
};

pub(crate) use self::{
	create::admin_create_token_route, delete::admin_delete_token_route,
	get::admin_get_token_route, list::admin_list_tokens_route, update::admin_update_token_route,
};

/// Map a validated token into the admin API wire shape. Config-file tokens
/// carry no usage state, so they read as unlimited, never expiring, and unused.
fn valid_token_response(valid: ValidToken) -> RegistrationToken {
	match valid.source {
		| ValidTokenSource::ConfigFile => RegistrationToken::new(valid.token),
		| ValidTokenSource::Database(info) => token_response(valid.token, &info),
	}
}

/// Map a token and its stored metadata into the admin API wire shape.
/// `pending` is always zero: tuwunel consumes uses in a single step with no
/// two-phase registration model.
fn token_response(token: String, info: &DatabaseTokenInfo) -> RegistrationToken {
	RegistrationToken {
		token,
		uses_allowed: info
			.expires
			.max_uses
			.and_then(|n| UInt::try_from(n).ok()),
		pending: uint!(0),
		completed: UInt::try_from(info.uses).unwrap_or(UInt::MAX),
		expiry_time: info
			.expires
			.max_age
			.and_then(MilliSecondsSinceUnixEpoch::from_system_time),
	}
}

fn token_expires(
	uses_allowed: Option<UInt>,
	expiry_time: Option<MilliSecondsSinceUnixEpoch>,
) -> Result<TokenExpires> {
	let max_age = expiry_time
		.map(|ms| timepoint_from_epoch(Duration::from_millis(ms.0.into())))
		.transpose()?;

	Ok(TokenExpires {
		max_uses: uses_allowed.map(Into::into),
		max_age,
	})
}

#[cfg(test)]
mod tests {
	use ruma::{MilliSecondsSinceUnixEpoch, uint};
	use tuwunel_service::registration_tokens::{DatabaseTokenInfo, TokenExpires};

	use super::token_response;

	#[test]
	fn unlimited_token_maps_to_null_cap_and_expiry() {
		let info = DatabaseTokenInfo {
			uses: 3,
			expires: TokenExpires { max_uses: None, max_age: None },
		};

		let token = token_response("abc".to_owned(), &info);

		assert_eq!(token.token, "abc");
		assert_eq!(token.completed, uint!(3));
		assert_eq!(token.pending, uint!(0));
		assert!(token.uses_allowed.is_none());
		assert!(token.expiry_time.is_none());
	}

	#[test]
	fn capped_token_maps_cap_and_expiry() {
		let expiry = MilliSecondsSinceUnixEpoch(
			1_595_376_300_000_u64
				.try_into()
				.expect("fits UInt"),
		);

		let expires_at = expiry.to_system_time().expect("valid timepoint");

		let info = DatabaseTokenInfo {
			uses: 10,
			expires: TokenExpires {
				max_uses: Some(100),
				max_age: Some(expires_at),
			},
		};

		let token = token_response("abc".to_owned(), &info);

		assert_eq!(token.uses_allowed, Some(uint!(100)));
		assert_eq!(token.completed, uint!(10));
		assert_eq!(token.expiry_time, Some(expiry));
	}
}
