mod dehydrated_device;
pub mod device;
mod keys;
mod ldap;
mod register;
mod sso;

use std::sync::Arc;

use futures::{Stream, StreamExt, TryFutureExt};
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedUserId, UserId,
	api::client::filter::FilterDefinition,
	events::{
		GlobalAccountDataEventType,
		ignored_user_list::IgnoredUserListEvent,
		invite_permission_config::{InvitePermissionAction, InvitePermissionConfigEvent},
	},
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Result, debug_warn, err, is_equal_to,
	matrix::pdu::PduCount,
	trace,
	utils::{self, ReadyExt, stream::TryIgnore},
};
use tuwunel_database::{Deserialized, Json, Map};

pub use self::{keys::parse_master_key, register::Register, sso::DeactivationReason};

pub const PASSWORD_SENTINEL: &str = "*";
pub const PASSWORD_DISABLED: &str = "";

/// Forensic record for a moderation action (MSC3823 suspend, MSC3939 lock).
/// Presence of the row is the load-bearing fact; this body is written but
/// never read on the hot path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Moderation {
	pub when: MilliSecondsSinceUnixEpoch,
	pub by: OwnedUserId,
}

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

struct Data {
	keychangeid_userid: Arc<Map>,
	keyid_key: Arc<Map>,
	onetimekeyid4225_otk: Option<Arc<Map>>,
	openidtoken_expiresatuserid: Arc<Map>,
	logintoken_expiresatuserid: Arc<Map>,
	todeviceid_events: Arc<Map>,
	spentrefresh_userdeviceid: Arc<Map>,
	token_userdeviceid: Arc<Map>,
	userdeviceid_metadata: Arc<Map>,
	userdeviceid_token: Arc<Map>,
	userdeviceidtoken_index: Arc<Map>,
	userdeviceid_refresh: Arc<Map>,
	userdeviceid_spentrefresh: Arc<Map>,
	userdeviceidalgorithm_fallback: Arc<Map>,
	oidcdevice_userdeviceid: Arc<Map>,
	oidccskeybypass_userid: Arc<Map>,
	userfilterid_filter: Arc<Map>,
	userid_dehydrateddevice: Arc<Map>,
	userid_devicelistversion: Arc<Map>,
	userid_erased: Arc<Map>,
	userid_deactivation_reason: Arc<Map>,
	userid_lastonetimekeyupdate: Arc<Map>,
	userid_locked: Arc<Map>,
	userid_masterkeyid: Arc<Map>,
	userid_password: Arc<Map>,
	userid_origin: Arc<Map>,
	userid_selfsigningkeyid: Arc<Map>,
	userid_suspended: Arc<Map>,
	userid_usersigningkeyid: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Data {
				keychangeid_userid: args.db["keychangeid_userid"].clone(),
				keyid_key: args.db["keyid_key"].clone(),
				onetimekeyid4225_otk: args.db.get("onetimekeyid4225_otk").ok().cloned(),
				openidtoken_expiresatuserid: args.db["openidtoken_expiresatuserid"].clone(),
				logintoken_expiresatuserid: args.db["logintoken_expiresatuserid"].clone(),
				oidcdevice_userdeviceid: args.db["oidcdevice_userdeviceid"].clone(),
				oidccskeybypass_userid: args.db["oidccskeybypass_userid"].clone(),
				todeviceid_events: args.db["todeviceid_events"].clone(),
				spentrefresh_userdeviceid: args.db["spentrefresh_userdeviceid"].clone(),
				token_userdeviceid: args.db["token_userdeviceid"].clone(),
				userdeviceid_metadata: args.db["userdeviceid_metadata"].clone(),
				userdeviceid_token: args.db["userdeviceid_token"].clone(),
				userdeviceidtoken_index: args.db["userdeviceidtoken_index"].clone(),
				userdeviceid_refresh: args.db["userdeviceid_refresh"].clone(),
				userdeviceid_spentrefresh: args.db["userdeviceid_spentrefresh"].clone(),
				userdeviceidalgorithm_fallback: args.db["userdeviceidalgorithm_fallback"].clone(),
				userfilterid_filter: args.db["userfilterid_filter"].clone(),
				userid_dehydrateddevice: args.db["userid_dehydrateddevice"].clone(),
				userid_devicelistversion: args.db["userid_devicelistversion"].clone(),
				userid_erased: args.db["userid_erased"].clone(),
				userid_deactivation_reason: args.db["userid_deactivation_reason"].clone(),
				userid_lastonetimekeyupdate: args.db["userid_lastonetimekeyupdate"].clone(),
				userid_locked: args.db["userid_locked"].clone(),
				userid_masterkeyid: args.db["userid_masterkeyid"].clone(),
				userid_password: args.db["userid_password"].clone(),
				userid_origin: args.db["userid_origin"].clone(),
				userid_selfsigningkeyid: args.db["userid_selfsigningkeyid"].clone(),
				userid_suspended: args.db["userid_suspended"].clone(),
				userid_usersigningkeyid: args.db["userid_usersigningkeyid"].clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Returns true/false based on whether the recipient/receiving user has
	/// blocked the sender
	pub async fn user_is_ignored(&self, sender_user: &UserId, recipient_user: &UserId) -> bool {
		self.services
			.account_data
			.get_global(recipient_user, GlobalAccountDataEventType::IgnoredUserList)
			.await
			.is_ok_and(|ignored: IgnoredUserListEvent| {
				ignored
					.content
					.ignored_users
					.keys()
					.any(|blocked_user| blocked_user == sender_user)
			})
	}

	/// MSC4380: `m.invite_permission_config.default_action == "block"`.
	pub async fn invites_blocked(&self, user_id: &UserId) -> bool {
		self.services
			.account_data
			.get_global(user_id, GlobalAccountDataEventType::InvitePermissionConfig)
			.await
			.is_ok_and(|event: InvitePermissionConfigEvent| {
				matches!(event.content.default_action, Some(InvitePermissionAction::Block))
			})
	}

	/// Create a new user account on this homeserver.
	///
	/// User origin is by default "password" (meaning that it will login using
	/// its user_id/password). Users with other origins (currently only "ldap"
	/// is available) have special login processes.
	#[inline]
	pub async fn create(
		&self,
		user_id: &UserId,
		password: Option<&str>,
		origin: Option<&str>,
	) -> Result {
		let origin = origin.unwrap_or("password");
		self.db.userid_origin.insert(user_id, origin);
		self.set_password(user_id, password).await
	}

	/// Deactivate account
	pub async fn deactivate_account(
		&self,
		user_id: &UserId,
		reason: DeactivationReason,
	) -> Result {
		// Revoke any SSO authorizations
		self.services
			.oauth
			.revoke_user_tokens(user_id)
			.await;

		// Remove all associated devices
		self.all_device_ids(user_id)
			.for_each(|device_id| self.remove_device(user_id, device_id))
			.await;

		// Set the password to "" to indicate a deactivated account. Hashes will never
		// result in an empty string, so the user will not be able to log in again.
		// Systems like changing the password without logging in should check if the
		// account is deactivated.
		self.set_password(user_id, None).await?;
		self.set_deactivation_reason(user_id, reason);

		// TODO: Unhook 3PID
		Ok(())
	}

	/// Check if a user has an account on this homeserver.
	#[inline]
	pub async fn exists(&self, user_id: &UserId) -> bool {
		self.db.userid_password.get(user_id).await.is_ok()
	}

	/// Check if account is deactivated
	pub async fn is_deactivated(&self, user_id: &UserId) -> Result<bool> {
		self.db
			.userid_password
			.get(user_id)
			.map_ok(|val| val.is_empty())
			.map_err(|_| err!(Request(NotFound("User does not exist."))))
			.await
	}

	/// Check if account is active, infallible
	pub async fn is_active(&self, user_id: &UserId) -> bool {
		!self.is_deactivated(user_id).await.unwrap_or(true)
	}

	/// Check if account is active, infallible
	pub async fn is_active_local(&self, user_id: &UserId) -> bool {
		self.services.globals.user_is_local(user_id) && self.is_active(user_id).await
	}

	/// MSC3823: account is suspended (read-mostly mode, sessions retained).
	pub async fn is_suspended(&self, user_id: &UserId) -> bool {
		self.db
			.userid_suspended
			.get(user_id)
			.await
			.is_ok()
	}

	/// MSC3939: account is locked (401 + soft_logout, sessions retained).
	pub async fn is_locked(&self, user_id: &UserId) -> bool {
		self.db.userid_locked.get(user_id).await.is_ok()
	}

	/// MSC4025: the user's events serve as pruned copies to recipients not
	/// joined at the event. Presence-only for the serving gate.
	pub async fn is_erased(&self, user_id: &UserId) -> bool {
		self.db.userid_erased.get(user_id).await.is_ok()
	}

	/// MSC4025: the global count recorded at erasure, for admin surfacing;
	/// the serving gate never reads it.
	pub async fn erasure_count(&self, user_id: &UserId) -> Option<PduCount> {
		self.db
			.userid_erased
			.get(user_id)
			.await
			.deserialized()
			.map(PduCount::from_unsigned)
			.ok()
	}

	/// MSC3823: forensic record for the active suspension, if any.
	pub async fn get_suspension(&self, user_id: &UserId) -> Option<Moderation> {
		self.db
			.userid_suspended
			.get(user_id)
			.await
			.deserialized::<Json<_>>()
			.map(|Json(m)| m)
			.ok()
	}

	/// MSC3939: forensic record for the active lock, if any.
	pub async fn get_lock(&self, user_id: &UserId) -> Option<Moderation> {
		self.db
			.userid_locked
			.get(user_id)
			.await
			.deserialized::<Json<_>>()
			.map(|Json(m)| m)
			.ok()
	}

	pub fn set_suspended(&self, user_id: &UserId, by: &UserId) {
		let entry = Moderation {
			when: MilliSecondsSinceUnixEpoch::now(),
			by: by.to_owned(),
		};

		self.db
			.userid_suspended
			.raw_put(user_id, Json(entry));
	}

	pub fn clear_suspended(&self, user_id: &UserId) { self.db.userid_suspended.remove(user_id); }

	/// MSC4025: mark the user erased, recording the current global count.
	pub fn set_erased(&self, user_id: &UserId) {
		let count = self.services.globals.current_count();

		self.db.userid_erased.raw_put(user_id, count);
	}

	/// MSC4025: erasure is reversible; clearing the marker restores the
	/// unredacted view.
	pub fn clear_erased(&self, user_id: &UserId) { self.db.userid_erased.remove(user_id); }

	pub fn set_locked(&self, user_id: &UserId, by: &UserId) {
		let entry = Moderation {
			when: MilliSecondsSinceUnixEpoch::now(),
			by: by.to_owned(),
		};

		self.db
			.userid_locked
			.raw_put(user_id, Json(entry));
	}

	pub fn clear_locked(&self, user_id: &UserId) { self.db.userid_locked.remove(user_id); }

	/// Returns the number of users registered on this server.
	#[inline]
	pub async fn count(&self) -> usize { self.db.userid_password.count().await }

	/// Returns an iterator over all users on this homeserver.
	pub fn stream(&self) -> impl Stream<Item = &UserId> + Send {
		self.db.userid_password.keys().ignore_err()
	}

	/// Returns a list of local users as list of usernames.
	///
	/// A user account is considered `local` if the length of it's password is
	/// greater then zero.
	pub fn list_local_users(&self) -> impl Stream<Item = &UserId> + Send + '_ {
		self.db
			.userid_password
			.stream()
			.ignore_err()
			.ready_filter_map(|(u, p): (&UserId, &[u8])| (!p.is_empty()).then_some(u))
	}

	/// Returns the origin of the user (password/LDAP/...).
	pub async fn origin(&self, user_id: &UserId) -> Result<String> {
		self.db
			.userid_origin
			.get(user_id)
			.await
			.deserialized()
	}

	/// Sets the origin of the user (password/sso/ldap/...).
	#[inline]
	pub fn set_origin(&self, user_id: &UserId, origin: &str) {
		self.db.userid_origin.insert(user_id, origin);
	}

	/// Returns whether the user has a password. Disabled accounts and
	/// registrations setting a sentinel password will return false here.
	pub async fn has_password(&self, user_id: &UserId) -> Result<bool> {
		self.password_hash(user_id)
			.map_ok(|value| value != PASSWORD_DISABLED && value != PASSWORD_SENTINEL)
			.await
	}

	/// Returns the password hash for the given user.
	pub async fn password_hash(&self, user_id: &UserId) -> Result<String> {
		self.db
			.userid_password
			.get(user_id)
			.await
			.deserialized()
	}

	/// Hash and set the user's password to the Argon2 hash
	pub async fn set_password(&self, user_id: &UserId, password: Option<&str>) -> Result {
		let keep_existing_origin = matches!(password, Some(PASSWORD_SENTINEL));

		// Cannot change the password of a LDAP user. There are two special cases :
		// - a `None` password can be used to deactivate a LDAP user
		// - a "*" password is used as the default password of an active LDAP user
		//
		// The above now applies to all non-password origin users by default unless an
		// exception is made for that origin in the condition below. Note that users
		// with no origin are also password-origin users.
		let allowed_origins = ["password", "sso"];
		if password.is_some() && password != Some(PASSWORD_SENTINEL) {
			let origin = self.origin(user_id).await;
			let origin = origin.as_deref().unwrap_or("password");

			if !allowed_origins.iter().any(is_equal_to!(&origin)) {
				return Err!(Request(InvalidParam(
					"Cannot change password of an {origin:?} user."
				)));
			}
		}

		match password.map(utils::hash::password) {
			| None => {
				self.db
					.userid_password
					.insert(user_id, PASSWORD_DISABLED);
			},
			| Some(Ok(_)) if password == Some(PASSWORD_SENTINEL) => {
				self.db
					.userid_password
					.insert(user_id, PASSWORD_SENTINEL);
			},
			| Some(Ok(hash)) => {
				self.db.userid_password.insert(user_id, hash);
				self.db.userid_deactivation_reason.remove(user_id);
				if !keep_existing_origin {
					self.db.userid_origin.insert(user_id, "password");
				}
			},
			| Some(Err(e)) => {
				return Err!(Request(InvalidParam(
					"Password does not meet the requirements: {e}"
				)));
			},
		}

		Ok(())
	}

	/// Creates a new sync filter. Returns the filter id.
	#[must_use]
	pub fn create_filter(&self, user_id: &UserId, filter: &FilterDefinition) -> String {
		let filter_id = utils::random_string(4);

		let key = (user_id, &filter_id);
		self.db.userfilterid_filter.put(key, Json(filter));

		filter_id
	}

	pub async fn get_filter(
		&self,
		user_id: &UserId,
		filter_id: &str,
	) -> Result<FilterDefinition> {
		let key = (user_id, filter_id);
		self.db
			.userfilterid_filter
			.qry(&key)
			.await
			.deserialized()
	}

	/// Creates an OpenID token, which can be used to prove that a user has
	/// access to an account (primarily for integrations)
	pub fn create_openid_token(&self, user_id: &UserId, token: &str) -> Result<u64> {
		use std::num::Saturating as Sat;

		let expires_in = self.services.server.config.openid_token_ttl;
		let expires_at = Sat(utils::millis_since_unix_epoch()) + Sat(expires_in) * Sat(1000);

		let mut value = expires_at.0.to_be_bytes().to_vec();
		value.extend_from_slice(user_id.as_bytes());

		self.db
			.openidtoken_expiresatuserid
			.insert(token.as_bytes(), value.as_slice());

		Ok(expires_in)
	}

	/// Find out which user an OpenID access token belongs to.
	pub async fn find_from_openid_token(&self, token: &str) -> Result<OwnedUserId> {
		let Ok(value) = self
			.db
			.openidtoken_expiresatuserid
			.get(token)
			.await
		else {
			return Err!(Request(Unauthorized("OpenID token is unrecognised")));
		};

		let (expires_at_bytes, user_bytes) = value.split_at(0_u64.to_be_bytes().len());
		let expires_at =
			u64::from_be_bytes(expires_at_bytes.try_into().map_err(|e| {
				err!(Database("expires_at in openid_userid is invalid u64. {e}"))
			})?);

		if expires_at < utils::millis_since_unix_epoch() {
			debug_warn!("OpenID token is expired, removing");
			self.db
				.openidtoken_expiresatuserid
				.remove(token.as_bytes());

			return Err!(Request(Unauthorized("OpenID token is expired")));
		}

		let user_string = utils::string_from_bytes(user_bytes)
			.map_err(|e| err!(Database("User ID in openid_userid is invalid unicode. {e}")))?;

		OwnedUserId::try_from(user_string)
			.map_err(|e| err!(Database("User ID in openid_userid is invalid. {e}")))
	}

	/// Creates a short-lived login token, which can be used to log in using the
	/// `m.login.token` mechanism.
	#[must_use]
	pub fn create_login_token(&self, user_id: &UserId, token: &str) -> u64 {
		use std::num::Saturating as Sat;

		let expires_in = self.services.server.config.login_token_ttl;
		let expires_at = Sat(utils::millis_since_unix_epoch()) + Sat(expires_in);

		let value = (expires_at.0, user_id);
		self.db
			.logintoken_expiresatuserid
			.raw_put(token, value);

		expires_in
	}

	/// Verify a login token is valid and return its owner without consuming it.
	/// Unlike `find_from_login_token`, the token remains in the database
	/// after this call and can still be consumed later.
	pub async fn peek_login_token(&self, token: &str) -> Result<OwnedUserId> {
		let Ok(value) = self
			.db
			.logintoken_expiresatuserid
			.get(token)
			.await
		else {
			return Err!(Request(Forbidden("Login token is unrecognised")));
		};
		let (expires_at, user_id): (u64, OwnedUserId) = value.deserialized()?;

		if expires_at < utils::millis_since_unix_epoch() {
			trace!(?user_id, ?token, "Removing expired login token");
			self.db.logintoken_expiresatuserid.remove(token);
			return Err!(Request(Forbidden("Login token is expired")));
		}

		Ok(user_id)
	}

	/// Find out which user a login token belongs to.
	/// Removes the token to prevent double-use attacks.
	pub async fn find_from_login_token(&self, token: &str) -> Result<OwnedUserId> {
		let Ok(value) = self
			.db
			.logintoken_expiresatuserid
			.get(token)
			.await
		else {
			return Err!(Request(Forbidden("Login token is unrecognised")));
		};
		let (expires_at, user_id): (u64, OwnedUserId) = value.deserialized()?;

		if expires_at < utils::millis_since_unix_epoch() {
			trace!(?user_id, ?token, "Removing expired login token");

			self.db.logintoken_expiresatuserid.remove(token);

			return Err!(Request(Forbidden("Login token is expired")));
		}

		self.db.logintoken_expiresatuserid.remove(token);

		Ok(user_id)
	}

	#[cfg(not(feature = "ldap"))]
	#[expect(clippy::unused_async)]
	pub async fn search_ldap(&self, _user_id: &UserId) -> Result<Vec<(String, bool)>> {
		Err!(FeatureDisabled("ldap"))
	}

	#[cfg(not(feature = "ldap"))]
	#[expect(clippy::unused_async)]
	pub async fn auth_ldap(&self, _user_dn: &str, _password: &str) -> Result {
		Err!(FeatureDisabled("ldap"))
	}
}
