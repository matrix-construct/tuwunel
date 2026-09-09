use axum::extract::State;
use ruma::{
	UserId,
	api::client::{
		keys::upload_signing_keys,
		uiaa::{AuthFlow, AuthType, UiaaInfo},
	},
	encryption::{CrossSigningKey, KeyUsage},
	serde::Raw,
};
use serde_json::{json, value::to_raw_value};
use tuwunel_core::{
	Err, Error, Result, debug, debug_error, err,
	result::NotFound,
	utils,
	utils::{BoolExt, OptionExt},
};
use tuwunel_service::{Services, uiaa::SESSION_ID_LENGTH, users::parse_master_key};

use crate::{Ruma, router::auth_uiaa};

struct Keys<'a> {
	user_id: &'a UserId,
	master_key: &'a Option<Raw<CrossSigningKey>>,
	self_signing_key: &'a Option<Raw<CrossSigningKey>>,
	user_signing_key: &'a Option<Raw<CrossSigningKey>>,
}

struct ValidatedKeys<'a>(Keys<'a>);

/// # `POST /_matrix/client/r0/keys/device_signing/upload`
///
/// Uploads end-to-end key information for the sender user.
///
/// - Requires UIAA to verify password
/// - For OIDC devices, requires OAuth re-authentication via SSO (MSC4312)
/// - For appservices with `device_management` enabled, UIAA is skipped even
///   when cross-signing keys already exist (MSC4190)
pub(crate) async fn upload_signing_keys_route(
	State(services): State<crate::State>,
	body: Ruma<upload_signing_keys::v3::Request>,
) -> Result<upload_signing_keys::v3::Response> {
	let sender_user = body.sender_user();

	let keys = validate_keys(Keys {
		user_id: sender_user,
		master_key: &body.master_key,
		self_signing_key: &body.self_signing_key,
		user_signing_key: &body.user_signing_key,
	})?;

	// Access token is required for this endpoint regardless of conditional UIAA so
	// we'll always have a sender_user.
	if let Ok(exists) = check_for_new_keys(
		&services,
		sender_user,
		body.self_signing_key.as_ref(),
		body.user_signing_key.as_ref(),
		body.master_key.as_ref(),
	)
	.await
	.inspect_err(|e| debug_error!(?e))
	{
		if let Some(result) = exists {
			// No-op, they tried to reupload the same set of keys
			// (lost connection for example)
			return Ok(result);
		}

		// Some of the keys weren't found, so we let them upload
		debug!("Skipping UIA as per MSC3967: user had no existing keys");
		return persist_signing_keys(&services, keys).await;
	}

	// MSC4190: appservices with device_management may replace existing
	// cross-signing keys without UIAA.
	if body
		.appservice_info
		.as_ref()
		.is_some_and(|appservice| appservice.registration.device_management)
	{
		debug!(
			"Skipping UIAA for {sender_user} as this is from an appservice and MSC4190 is \
			 enabled"
		);

		return persist_signing_keys(&services, keys).await;
	}

	let is_oidc = body
		.sender_device()
		.ok()
		.map_async(|sender_device| {
			services
				.users
				.is_oidc_device(sender_user, sender_device)
		})
		.await
		.unwrap_or(false);

	// MSC4312: OIDC devices require OAuth re-authentication for cross-signing
	// reset. If a bypass was granted via SSO re-auth, skip UIAA entirely.
	if is_oidc
		&& services
			.users
			.can_replace_cross_signing_keys(sender_user)
			.await
	{
		return persist_signing_keys(&services, keys).await;
	}

	// First attempt from OIDC device: issue m.oauth flow.
	if is_oidc && body.auth.is_none() {
		return Err(Error::Uiaa(create_oauth_uiaa(&services, sender_user, &body)?));
	}

	let authed_user = auth_uiaa(&services, &body).await?;

	assert_eq!(sender_user, authed_user, "Expected UIAA of {sender_user} and not {authed_user}");
	persist_signing_keys(&services, keys).await
}

fn validate_keys(keys: Keys<'_>) -> Result<ValidatedKeys<'_>> {
	[
		(keys.master_key.as_ref(), KeyUsage::Master),
		(keys.self_signing_key.as_ref(), KeyUsage::SelfSigning),
		(keys.user_signing_key.as_ref(), KeyUsage::UserSigning),
	]
	.into_iter()
	.try_for_each(|(key, usage)| validate_key(keys.user_id, key, &usage))?;

	Ok(ValidatedKeys(keys))
}

fn validate_key(
	user_id: &UserId,
	key: Option<&Raw<CrossSigningKey>>,
	usage: &KeyUsage,
) -> Result {
	let Some(key) = key else {
		return Ok(());
	};

	let key = key
		.deserialize()
		.map_err(|error| err!(Request(InvalidParam("Invalid cross-signing key: {error}"))))?;

	if key.user_id != user_id {
		return Err!(Request(InvalidParam("Cross-signing key belongs to another user.")));
	}

	if !key.usage.contains(usage) {
		return Err!(Request(InvalidParam(
			"Cross-signing key does not include the required usage."
		)));
	}

	if key.keys.len() != 1 {
		return Err!(Request(InvalidParam("Cross-signing key must contain exactly one key.")));
	}

	Ok(())
}

async fn persist_signing_keys(
	services: &Services,
	keys: ValidatedKeys<'_>,
) -> Result<upload_signing_keys::v3::Response> {
	let ValidatedKeys(keys) = keys;

	services
		.users
		.add_cross_signing_keys(
			keys.user_id,
			keys.master_key,
			keys.self_signing_key,
			keys.user_signing_key,
			true, // notify so that other users see the new keys
		)
		.await?;

	Ok(upload_signing_keys::v3::Response {})
}

fn create_oauth_uiaa(
	services: &Services,
	sender_user: &UserId,
	body: &Ruma<upload_signing_keys::v3::Request>,
) -> Result<UiaaInfo> {
	let session = utils::random_string(SESSION_ID_LENGTH);
	let issuer = services.oauth.get_server()?.issuer_url()?;
	let base = issuer.trim_end_matches('/');
	let url = format!("{base}/_tuwunel/oidc/account?action=org.matrix.cross_signing_reset");

	let uiaainfo = UiaaInfo {
		flows: vec![AuthFlow { stages: vec![AuthType::OAuth] }],
		params: Some(to_raw_value(&json!({"m.oauth": { "url": url }}))?),
		session: Some(session),
		..Default::default()
	};

	services.uiaa.create(
		sender_user,
		body.sender_device()?,
		&uiaainfo,
		body.json_body
			.as_ref()
			.ok_or_else(|| err!(Request(NotJson("JSON body is not valid"))))?,
	);

	Ok(uiaainfo)
}

async fn check_for_new_keys(
	services: &Services,
	user_id: &UserId,
	self_signing_key: Option<&Raw<CrossSigningKey>>,
	user_signing_key: Option<&Raw<CrossSigningKey>>,
	master_signing_key: Option<&Raw<CrossSigningKey>>,
) -> Result<Option<upload_signing_keys::v3::Response>> {
	debug!("checking for existing keys");

	let empty = match master_signing_key {
		| Some(new_master) => !master_key_matches(services, user_id, new_master).await?,
		| None => false,
	};

	if let Some(new_user_signing) = user_signing_key {
		let fetched = services.users.get_user_signing_key(user_id).await;

		if fetched.is_not_found() {
			if !empty {
				return Err!(Request(Forbidden(
					"Tried to update an existing user signing key, UIA required"
				)));
			}
		} else if fetched?.deserialize()? != new_user_signing.deserialize()? {
			return Err!(Request(Forbidden(
				"Tried to change an existing user signing key, UIA required"
			)));
		}
	}

	if let Some(new_self_signing) = self_signing_key {
		let fetched = services
			.users
			.get_self_signing_key(None, user_id, &|_| true)
			.await;

		if fetched.is_not_found() {
			if !empty {
				return Err!(Request(Forbidden(
					"Tried to add a new signing key independently from the master key"
				)));
			}
		} else if fetched?.deserialize()? != new_self_signing.deserialize()? {
			return Err!(Request(Forbidden(
				"Tried to update an existing self signing key, UIA required"
			)));
		}
	}

	Ok(empty
		.is_false()
		.into_option()
		.map(|()| upload_signing_keys::v3::Response {}))
}

/// Returns `true` if the user already has a master key matching `new_master`,
/// `false` if they have no master key. Returns `Err` on mismatch or any other
/// error.
async fn master_key_matches(
	services: &Services,
	user_id: &UserId,
	new_master: &Raw<CrossSigningKey>,
) -> Result<bool> {
	let (new_id, new_value) = parse_master_key(user_id, new_master)?;
	let existing = services
		.users
		.get_master_key(None, user_id, &|_| true)
		.await;

	if existing.is_not_found() {
		return Ok(false);
	}

	let (existing_id, existing_value) = parse_master_key(user_id, &existing?)?;
	if existing_id != new_id || existing_value != new_value {
		return Err!(Request(Forbidden("Tried to change an existing master key, UIA required")));
	}

	Ok(true)
}

#[cfg(test)]
mod tests {
	use ruma::{api::error::ErrorKind::InvalidParam, user_id};
	use serde_json::Map;

	use super::*;

	#[test]
	fn accepts_valid_unsigned_keys_and_additional_usages() {
		let user_id = user_id!("@alice:example.com");
		let master_key =
			Some(signing_key(user_id, &["master", "self_signing"], &["ed25519:master"]));

		let self_signing_key =
			Some(signing_key(user_id, &["self_signing", "user_signing"], &["ed25519:self"]));

		let user_signing_key =
			Some(signing_key(user_id, &["user_signing", "master"], &["ed25519:user"]));

		validate_keys(Keys {
			user_id,
			master_key: &master_key,
			self_signing_key: &self_signing_key,
			user_signing_key: &user_signing_key,
		})
		.expect("valid unsigned keys should pass structural validation");
	}

	#[test]
	fn rejects_wrong_owner_for_each_role() {
		let user_id = user_id!("@alice:example.com");
		let other_user = user_id!("@mallory:elsewhere.example");
		let none = None;
		let master_key = Some(signing_key(other_user, &["master"], &["ed25519:master"]));
		let self_signing_key =
			Some(signing_key(other_user, &["self_signing"], &["ed25519:self"]));

		let user_signing_key =
			Some(signing_key(other_user, &["user_signing"], &["ed25519:user"]));

		assert_invalid(&validate_keys(Keys {
			user_id,
			master_key: &master_key,
			self_signing_key: &none,
			user_signing_key: &none,
		}));

		assert_invalid(&validate_keys(Keys {
			user_id,
			master_key: &none,
			self_signing_key: &self_signing_key,
			user_signing_key: &none,
		}));

		assert_invalid(&validate_keys(Keys {
			user_id,
			master_key: &none,
			self_signing_key: &none,
			user_signing_key: &user_signing_key,
		}));
	}

	#[test]
	fn rejects_missing_usage_for_each_role() {
		let user_id = user_id!("@alice:example.com");
		let none = None;
		let master_key = Some(signing_key(user_id, &["self_signing"], &["ed25519:master"]));
		let self_signing_key = Some(signing_key(user_id, &["master"], &["ed25519:self"]));
		let user_signing_key = Some(signing_key(user_id, &["master"], &["ed25519:user"]));

		assert_invalid(&validate_keys(Keys {
			user_id,
			master_key: &master_key,
			self_signing_key: &none,
			user_signing_key: &none,
		}));

		assert_invalid(&validate_keys(Keys {
			user_id,
			master_key: &none,
			self_signing_key: &self_signing_key,
			user_signing_key: &none,
		}));

		assert_invalid(&validate_keys(Keys {
			user_id,
			master_key: &none,
			self_signing_key: &none,
			user_signing_key: &user_signing_key,
		}));
	}

	#[test]
	fn rejects_invalid_key_counts() {
		let user_id = user_id!("@alice:example.com");
		let none = None;
		let empty = Some(signing_key(user_id, &["master"], &[]));
		let multiple =
			Some(signing_key(user_id, &["self_signing"], &["ed25519:first", "ed25519:second"]));

		assert_invalid(&validate_keys(Keys {
			user_id,
			master_key: &empty,
			self_signing_key: &none,
			user_signing_key: &none,
		}));

		assert_invalid(&validate_keys(Keys {
			user_id,
			master_key: &none,
			self_signing_key: &multiple,
			user_signing_key: &none,
		}));
	}

	#[test]
	fn rejects_a_malformed_later_key_before_persistence() {
		let user_id = user_id!("@alice:example.com");
		let master_key = Some(signing_key(user_id, &["master"], &["ed25519:master"]));
		let self_signing_key = Some(signing_key(user_id, &["self_signing"], &["ed25519:self"]));
		let user_signing_key = Some(signing_key(user_id, &["master"], &["ed25519:user"]));

		assert_invalid(&validate_keys(Keys {
			user_id,
			master_key: &master_key,
			self_signing_key: &self_signing_key,
			user_signing_key: &user_signing_key,
		}));
	}

	fn signing_key(user_id: &UserId, usage: &[&str], key_ids: &[&str]) -> Raw<CrossSigningKey> {
		let keys: Map<_, _> = key_ids
			.iter()
			.map(|key_id| ((*key_id).to_owned(), json!("public-key")))
			.collect();

		let key = json!({
			"user_id": user_id,
			"usage": usage,
			"keys": keys,
		});

		Raw::from_json(to_raw_value(&key).expect("cross-signing key should serialize"))
	}

	fn assert_invalid(result: &Result<ValidatedKeys<'_>>) {
		assert!(matches!(result, Err(Error::Request(InvalidParam, ..))));
	}
}
