use std::time::{Duration, SystemTime};

use ruma::{OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Err, Result, err, implement, smallstr::SmallString, utils};
use tuwunel_database::{Cbor, Deserialized};

/// A pending RFC 8628 device authorization grant, keyed in the store by its
/// `device_code` and reachable for the browser approval step through a separate
/// `user_code` index.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceGrant {
	pub device_code: String,

	/// Stored in its normalized form (the index key); display it with
	/// [`format_user_code`].
	pub user_code: String,

	pub client_id: String,
	pub scope: String,

	#[serde(default)]
	authentication: Option<Authentication>,

	pub status: DeviceGrantStatus,
	pub attempts: u32,
	pub created_at: SystemTime,
	pub expires_at: SystemTime,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Authentication {
	user_id: OwnedUserId,
	idp_id: Option<ProviderId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum DeviceGrantStatus {
	Pending,
	Approved {
		user_id: OwnedUserId,
		idp_id: Option<String>,
	},
	Denied,
}

/// The approved grant data the token endpoint consumes to issue tokens.
pub struct ApprovedDeviceGrant {
	pub client_id: String,
	pub scope: String,
	pub user_id: OwnedUserId,
	pub idp_id: Option<String>,
}

/// The outcome of a token-endpoint poll, mapped to the RFC 8628 §3.5 error
/// codes by the caller.
pub enum DeviceGrantPoll {
	Pending,
	Approved(ApprovedDeviceGrant),
	Denied,
	Expired,
}

const DEVICE_CODE_LENGTH: usize = 64;

/// `user_code` length. Ten base-20 characters is ~43 bits: with the always-on
/// per-IP throttle over the grant lifetime that keeps a single source well
/// under the RFC 8628 §5.1 brute-force ceiling, while staying short enough to
/// type.
const USER_CODE_LENGTH: usize = 10;

/// RFC 8628 §6.1 base-20 alphabet: uppercase consonants only, so a user can
/// type the code without modifier keys and without forming words or hitting a
/// confusable digit.
const USER_CODE_CHARSET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

/// Cap on how many times one grant's `user_code` may be brought to the consent
/// step before it self-invalidates (RFC 8628 §5.1 per-code attempt cap / §5.4
/// possession limit), generous enough for a page reload.
const MAX_VERIFY_ATTEMPTS: u32 = 10;
type ProviderId = SmallString<[u8; 32]>;

pub const DEVICE_GRANT_LIFETIME: Duration = Duration::from_mins(30);
pub const DEVICE_GRANT_INTERVAL_SECS: u64 = 5;

#[implement(super::Server)]
#[must_use]
pub fn create_device_grant(&self, client_id: &str, scope: &str) -> DeviceGrant {
	let now = SystemTime::now();
	let device_code = utils::random_string(DEVICE_CODE_LENGTH);
	let user_code = utils::random_string_from(USER_CODE_CHARSET, USER_CODE_LENGTH);
	let grant = DeviceGrant {
		device_code: device_code.clone(),
		user_code: user_code.clone(),
		client_id: client_id.to_owned(),
		scope: scope.to_owned(),
		authentication: None,
		status: DeviceGrantStatus::Pending,
		attempts: 0,
		created_at: now,
		expires_at: now
			.checked_add(DEVICE_GRANT_LIFETIME)
			.unwrap_or(now),
	};

	self.db
		.oidcdevicecode_devicegrant
		.raw_put(&*device_code, Cbor(&grant));

	self.db
		.oidcusercode_devicecode
		.raw_put(&*user_code, Cbor(&device_code));

	grant
}

/// Resolve a user-entered (possibly hyphenated, lower-case) code to its stored
/// `device_code` via the index.
#[implement(super::Server)]
async fn resolve_device_code(&self, user_code: &str) -> Result<String> {
	let user_code = normalize_user_code(user_code);

	self.db
		.oidcusercode_devicecode
		.get(&user_code)
		.await
		.deserialized::<Cbor<_>>()
		.map(|cbor: Cbor<String>| cbor.0)
		.map_err(|_| err!(Request(NotFound("Unknown or expired user code"))))
}

#[implement(super::Server)]
fn ensure_pending_device_grant(&self, grant: &DeviceGrant) -> Result {
	if SystemTime::now() > grant.expires_at {
		self.remove_device_grant(&grant.device_code, &grant.user_code);

		return Err!(Request(NotFound("The device authorization has expired")));
	}

	if !matches!(grant.status, DeviceGrantStatus::Pending) {
		return Err!(Request(Forbidden("The device authorization was already resolved")));
	}

	Ok(())
}

fn device_authentication_matches(
	grant: &DeviceGrant,
	user_id: &UserId,
	idp_id: Option<&str>,
) -> bool {
	grant
		.authentication
		.as_ref()
		.is_some_and(|authentication| {
			authentication.user_id == user_id && authentication.idp_id.as_deref() == idp_id
		})
}

#[implement(super::Server)]
fn bind_device_authentication(
	&self,
	mut grant: DeviceGrant,
	authentication: Authentication,
) -> Result<DeviceGrant> {
	if grant
		.authentication
		.as_ref()
		.is_some_and(|bound| bound != &authentication)
	{
		return Err!(Request(Forbidden(
			"The device authorization was authenticated by another user or provider"
		)));
	}

	grant.attempts = grant.attempts.saturating_add(1);
	if grant.attempts > MAX_VERIFY_ATTEMPTS {
		self.remove_device_grant(&grant.device_code, &grant.user_code);

		return Err!(Request(Forbidden("Too many attempts; request a new code")));
	}

	grant.authentication = Some(authentication);
	Ok(grant)
}

/// Look up a pending grant for the browser consent step, counting the attempt
/// and self-invalidating the grant past `MAX_VERIFY_ATTEMPTS` (RFC 8628 §5.1).
///
/// The first successful lookup binds the grant to its authenticated user and
/// provider. Later lookups must present the same authentication.
#[implement(super::Server)]
pub async fn verify_device_grant(
	&self,
	user_code: &str,
	user_id: &UserId,
	idp_id: Option<&str>,
) -> Result<DeviceGrant> {
	let device_code = self.resolve_device_code(user_code).await?;
	let _lock = self.device_locks.lock(&device_code).await;

	let grant = self.get_device_grant(&device_code).await?;
	let authentication = Authentication {
		user_id: user_id.to_owned(),
		idp_id: idp_id.map(Into::into),
	};

	self.ensure_pending_device_grant(&grant)?;
	let grant = self.bind_device_authentication(grant, authentication)?;
	self.db
		.oidcdevicecode_devicegrant
		.raw_put(&*grant.device_code, Cbor(&grant));

	Ok(grant)
}

#[implement(super::Server)]
pub async fn approve_device_grant(
	&self,
	user_code: &str,
	user_id: OwnedUserId,
	idp_id: Option<String>,
) -> Result {
	let device_code = self.resolve_device_code(user_code).await?;
	let _lock = self.device_locks.lock(&device_code).await;
	let grant = self.get_device_grant(&device_code).await?;

	self.ensure_pending_device_grant(&grant)?;

	if !device_authentication_matches(&grant, &user_id, idp_id.as_deref()) {
		return Err!(Request(Forbidden(
			"The device authorization consent does not match its authenticated session"
		)));
	}

	let grant = DeviceGrant {
		status: DeviceGrantStatus::Approved { user_id, idp_id },
		..grant
	};

	self.db
		.oidcdevicecode_devicegrant
		.raw_put(&*grant.device_code, Cbor(&grant));

	Ok(())
}

#[implement(super::Server)]
pub async fn deny_device_grant(
	&self,
	user_code: &str,
	user_id: OwnedUserId,
	idp_id: Option<String>,
) -> Result {
	let device_code = self.resolve_device_code(user_code).await?;
	let _lock = self.device_locks.lock(&device_code).await;
	let grant = self.get_device_grant(&device_code).await?;

	self.ensure_pending_device_grant(&grant)?;

	if !device_authentication_matches(&grant, &user_id, idp_id.as_deref()) {
		return Err!(Request(Forbidden(
			"The device authorization consent does not match its authenticated session"
		)));
	}

	let grant = DeviceGrant {
		status: DeviceGrantStatus::Denied,
		..grant
	};

	self.db
		.oidcdevicecode_devicegrant
		.raw_put(&*grant.device_code, Cbor(&grant));

	Ok(())
}

/// Poll a device grant by its `device_code` (RFC 8628 §3.4). A terminal outcome
/// consumes the grant; a pending grant is left in place for the next poll.
#[implement(super::Server)]
pub async fn poll_device_grant(
	&self,
	device_code: &str,
	client_id: &str,
) -> Result<DeviceGrantPoll> {
	// Serialize the read-check-consume so two concurrent polls of one approved
	// grant cannot both reach issuance.
	let _lock = self.device_locks.lock(device_code).await;

	let grant = self.get_device_grant(device_code).await?;

	if grant.client_id != client_id {
		return Err!(Request(Forbidden("client_id mismatch")));
	}

	if SystemTime::now() > grant.expires_at {
		self.remove_device_grant(&grant.device_code, &grant.user_code);

		return Ok(DeviceGrantPoll::Expired);
	}

	match grant.status {
		| DeviceGrantStatus::Pending => Ok(DeviceGrantPoll::Pending),
		| DeviceGrantStatus::Denied => {
			self.remove_device_grant(&grant.device_code, &grant.user_code);

			Ok(DeviceGrantPoll::Denied)
		},
		| DeviceGrantStatus::Approved { user_id, idp_id } => {
			self.remove_device_grant(&grant.device_code, &grant.user_code);

			Ok(DeviceGrantPoll::Approved(ApprovedDeviceGrant {
				client_id: grant.client_id,
				scope: grant.scope,
				user_id,
				idp_id,
			}))
		},
	}
}

#[implement(super::Server)]
async fn get_device_grant(&self, device_code: &str) -> Result<DeviceGrant> {
	self.db
		.oidcdevicecode_devicegrant
		.get(device_code)
		.await
		.deserialized::<Cbor<_>>()
		.map(|cbor: Cbor<DeviceGrant>| cbor.0)
		.map_err(|_| err!(Request(Forbidden("Invalid or expired device code"))))
}

#[implement(super::Server)]
fn remove_device_grant(&self, device_code: &str, user_code: &str) {
	self.db
		.oidcdevicecode_devicegrant
		.remove(device_code);
	self.db.oidcusercode_devicecode.remove(user_code);
}

/// Fold user input back to the stored form: uppercase, keeping only charset
/// bytes (RFC 8628 §6.1) so hyphens, spaces and case do not defeat the lookup.
fn normalize_user_code(input: &str) -> String {
	input
		.bytes()
		.map(|b| b.to_ascii_uppercase())
		.filter(|b| USER_CODE_CHARSET.contains(b))
		.map(char::from)
		.collect()
}

/// Render a stored user code for display, grouped with a single hyphen.
#[must_use]
pub fn format_user_code(code: &str) -> String {
	code.split_at_checked(code.len() / 2)
		.filter(|(head, tail)| !head.is_empty() && !tail.is_empty())
		.map(|(head, tail)| format!("{head}-{tail}"))
		.unwrap_or_else(|| code.to_owned())
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, SystemTime};

	use serde::Serialize;
	use tuwunel_database::{Cbor, deserialize_from_slice, serialize_to_vec};

	use super::{
		DeviceGrant, DeviceGrantStatus, USER_CODE_CHARSET, USER_CODE_LENGTH, format_user_code,
		normalize_user_code,
	};

	#[derive(Serialize)]
	struct LegacyDeviceGrant<'a> {
		device_code: &'a str,
		user_code: &'a str,
		client_id: &'a str,
		scope: &'a str,
		status: DeviceGrantStatus,
		attempts: u32,
		created_at: SystemTime,
		expires_at: SystemTime,
	}

	#[test]
	fn legacy_grant_defaults_to_no_authentication() {
		let created_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
		let legacy = LegacyDeviceGrant {
			device_code: "device",
			user_code: "BCDFGHJKLM",
			client_id: "client",
			scope: "openid",
			status: DeviceGrantStatus::Pending,
			attempts: 0,
			created_at,
			expires_at: created_at + Duration::from_secs(30),
		};

		let bytes = serialize_to_vec(Cbor(&legacy)).expect("serialize legacy grant");
		let Cbor(grant): Cbor<DeviceGrant> =
			deserialize_from_slice(&bytes).expect("deserialize legacy grant");

		assert!(grant.authentication.is_none());
	}

	#[test]
	fn format_then_normalize_round_trips() {
		let code = "BCDFGHJK";

		assert_eq!(normalize_user_code(&format_user_code(code)), code);
	}

	#[test]
	fn normalize_strips_separators_and_uppercases() {
		assert_eq!(normalize_user_code("bcdf-ghjk"), "BCDFGHJK");
		assert_eq!(normalize_user_code(" bc df ghjk "), "BCDFGHJK");
	}

	#[test]
	fn normalize_drops_out_of_charset_characters() {
		assert_eq!(normalize_user_code("B0C1DAEF"), "BCDF");
	}

	#[test]
	fn format_inserts_a_single_separator() {
		assert_eq!(format_user_code("BCDFGHJK"), "BCDF-GHJK");
	}

	#[test]
	fn charset_is_base20_without_vowels_or_digits() {
		assert_eq!(USER_CODE_CHARSET.len(), 20);
		assert_eq!(USER_CODE_LENGTH, 10);

		// RFC 8628 drops the vowels and Y; it keeps every other consonant.
		for excluded in b"AEIOUY0123456789" {
			assert!(!USER_CODE_CHARSET.contains(excluded));
		}
	}
}
