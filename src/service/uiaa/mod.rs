use std::{
	collections::BTreeMap,
	ops::ControlFlow,
	sync::{Arc, RwLock},
};

use futures::{TryStreamExt, pin_mut};
use ruma::{
	CanonicalJsonValue, DeviceId, OwnedDeviceId, OwnedUserId, UserId,
	api::{
		client::uiaa::{
			AuthData, AuthType, EmailIdentity, Password, ThirdpartyIdCredentials, UiaaInfo,
			UserIdentifier,
		},
		error::{ErrorKind, StandardErrorBody},
	},
};
use tuwunel_core::{
	Err, Result, err, error, extract, implement,
	utils::{self, BoolExt, hash::verify_password, string::EMPTY},
};
use tuwunel_database::{Deserialized, Json, Map};

pub struct Service {
	userdevicesessionid_uiaarequest: RwLock<RequestMap>,
	db: Data,
	services: Arc<crate::services::OnceServices>,
}

struct Data {
	userdevicesessionid_uiaainfo: Arc<Map>,
}

type RequestMap = BTreeMap<RequestKey, CanonicalJsonValue>;
type RequestKey = (OwnedUserId, OwnedDeviceId, String);

pub const SESSION_ID_LENGTH: usize = 32;

#[derive(Clone, Copy)]
enum EmailIdentityMode {
	Validate,
	Claim,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			userdevicesessionid_uiaarequest: RwLock::new(RequestMap::new()),
			db: Data {
				userdevicesessionid_uiaainfo: args.db["userdevicesessionid_uiaainfo"].clone(),
			},
			services: args.services.clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Creates a new Uiaa session. Make sure the session token is unique.
#[implement(Service)]
pub fn create(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	uiaainfo: &UiaaInfo,
	json_body: &CanonicalJsonValue,
) {
	// TODO: better session error handling (why is uiaainfo.session optional in
	// ruma?)
	let session = uiaainfo
		.session
		.as_ref()
		.expect("session should be set");

	self.set_uiaa_request(user_id, device_id, session, json_body);

	self.update_uiaa_session(user_id, device_id, session, Some(uiaainfo));
}

/// Authenticate one stage without taking ownership of an email proof.
///
/// Generic UIAA consumers may validate email identity, but only registration
/// assigns a durable owner to that proof.
#[implement(Service)]
pub async fn try_auth(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	auth: &AuthData,
	uiaainfo: &UiaaInfo,
) -> Result<(bool, UiaaInfo)> {
	self.try_auth_inner(user_id, device_id, auth, uiaainfo, EmailIdentityMode::Validate)
		.await
}

/// Authenticate one registration stage and claim an email proof when present.
///
/// The claim is tied to the exact user, device, and UIAA session tuple before
/// the email stage is recorded as complete.
#[implement(Service)]
pub async fn try_auth_registration(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	auth: &AuthData,
	uiaainfo: &UiaaInfo,
) -> Result<(bool, UiaaInfo)> {
	self.try_auth_inner(user_id, device_id, auth, uiaainfo, EmailIdentityMode::Claim)
		.await
}

#[implement(Service)]
async fn try_auth_inner(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	auth: &AuthData,
	uiaainfo: &UiaaInfo,
	email_identity_mode: EmailIdentityMode,
) -> Result<(bool, UiaaInfo)> {
	let mut uiaainfo = if let Some(session) = auth.session() {
		self.get_uiaa_session(user_id, device_id, session)
			.await?
	} else {
		uiaainfo.clone()
	};

	if uiaainfo.session.is_none() {
		uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
	}

	match auth {
		// Find out what the user completed
		| AuthData::Password(password) => {
			if let ControlFlow::Break(authed) = self
				.verify_password(user_id, &mut uiaainfo, password)
				.await?
			{
				return Ok((authed, uiaainfo));
			}
		},
		| AuthData::RegistrationToken(t) => {
			let token = t.token.trim();
			if self
				.services
				.registration_tokens
				.try_consume(token)
				.await
				.is_ok()
			{
				uiaainfo
					.completed
					.push(AuthType::RegistrationToken);
			} else {
				uiaainfo.auth_error = Some(Box::new(StandardErrorBody {
					kind: ErrorKind::forbidden(),
					message: "Invalid registration token.".to_owned(),
				}));

				return Ok((false, uiaainfo));
			}
		},
		| AuthData::FallbackAcknowledgement(_session) => {
			// A fallback acknowledgement is a session re-poll. The fallback
			// web handler (e.g. the SSO callback) is what records completion.
		},
		| AuthData::OAuth(_) => {
			// MSC4312: OAuth cross-signing reset uses SSO re-authentication.
			// If a bypass was granted via SSO re-auth, mark OAuth as completed.
			if !uiaainfo.completed.contains(&AuthType::OAuth) {
				if self
					.services
					.users
					.can_replace_cross_signing_keys(user_id)
					.await
				{
					uiaainfo.completed.push(AuthType::OAuth);
				} else {
					uiaainfo.auth_error = Some(Box::new(StandardErrorBody {
						kind: ErrorKind::forbidden(),
						message: "OAuth cross-signing reset not approved for this session."
							.to_owned(),
					}));

					return Ok((false, uiaainfo));
				}
			}
		},
		| AuthData::Dummy(_) => {
			uiaainfo.completed.push(AuthType::Dummy);
		},
		| AuthData::Terms(_) => {
			// MSC1692: an empty auth dict accepts every presented policy.
			uiaainfo.completed.push(AuthType::Terms);
		},
		| AuthData::EmailIdentity(EmailIdentity { thirdparty_id_creds, .. }) => {
			// A stray id_server is tolerated and id_access_token is never required.
			let validated = self
				.authenticate_email_identity(
					user_id,
					device_id,
					&uiaainfo,
					thirdparty_id_creds,
					email_identity_mode,
				)
				.await?;

			if !validated {
				uiaainfo.auth_error = Some(Box::new(StandardErrorBody {
					kind: ErrorKind::forbidden(),
					message: "Email address has not been validated.".to_owned(),
				}));

				return Ok((false, uiaainfo));
			}

			uiaainfo.completed.push(AuthType::EmailIdentity);
		},
		| auth => error!("AuthData type not supported: {auth:?}"),
	}

	// Check if a flow now succeeds
	let mut completed = false;
	'flows: for flow in &mut uiaainfo.flows {
		for stage in &flow.stages {
			if !uiaainfo.completed.contains(stage) {
				continue 'flows;
			}
		}
		// We didn't break, so this flow succeeded!
		completed = true;
	}

	let session = uiaainfo
		.session
		.as_ref()
		.expect("session is always set");

	if matches!(email_identity_mode, EmailIdentityMode::Claim)
		&& !matches!(auth, AuthData::EmailIdentity(_))
		&& uiaainfo
			.completed
			.contains(&AuthType::EmailIdentity)
	{
		let claim = (user_id.to_owned(), device_id.to_owned(), session.as_str().into());

		if !self
			.services
			.threepid
			.refresh_claim(&claim)
			.await?
		{
			uiaainfo
				.completed
				.retain(|stage| stage != &AuthType::EmailIdentity);

			uiaainfo.auth_error = Some(Box::new(StandardErrorBody {
				kind: ErrorKind::forbidden(),
				message: "Email address has not been validated.".to_owned(),
			}));

			self.update_uiaa_session(user_id, device_id, session, Some(&uiaainfo));

			return Ok((false, uiaainfo));
		}
	}

	if !completed {
		self.update_uiaa_session(user_id, device_id, session, Some(&uiaainfo));

		return Ok((false, uiaainfo));
	}

	// Retain the session until registration spends its email claim.
	let retain_session = matches!(email_identity_mode, EmailIdentityMode::Claim)
		&& uiaainfo
			.completed
			.contains(&AuthType::EmailIdentity);

	self.update_uiaa_session(user_id, device_id, session, retain_session.then_some(&uiaainfo));

	Ok((true, uiaainfo))
}

#[implement(Service)]
async fn authenticate_email_identity(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	uiaainfo: &UiaaInfo,
	creds: &ThirdpartyIdCredentials,
	mode: EmailIdentityMode,
) -> Result<bool> {
	match mode {
		| EmailIdentityMode::Validate => Ok(self
			.services
			.threepid
			.session_validated(creds.sid.as_str(), creds.client_secret.as_str())
			.await),
		| EmailIdentityMode::Claim => {
			let session = uiaainfo
				.session
				.as_ref()
				.expect("session is always set");

			let claim = (user_id.to_owned(), device_id.to_owned(), session.as_str().into());

			self.services
				.threepid
				.claim_validated(creds.sid.as_str(), creds.client_secret.as_str(), claim)
				.await
		},
	}
}

#[implement(Service)]
async fn verify_password(
	&self,
	user_id: &UserId,
	uiaainfo: &mut UiaaInfo,
	password: &Password,
) -> Result<ControlFlow<bool>> {
	let Password { identifier, password, user, .. } = password;

	let username = extract!(identifier, x in Some(UserIdentifier::Matrix(ruma::api::client::uiaa::MatrixUserIdentifier { user: x, .. })))
		.or_else(|| cfg!(feature = "element_hacks").and(user.as_ref()))
		.ok_or(err!(Request(Unrecognized("Identifier type not recognized."))))?;

	let user_id_from_username =
		UserId::parse_with_server_name(username.clone(), self.services.globals.server_name())
			.map_err(|_| err!(Request(InvalidParam("User ID is invalid."))))?;

	// Check if the access token being used matches the credentials used for UIAA
	if user_id.localpart() != user_id_from_username.localpart() {
		return Err!(Request(Forbidden("User ID and access token mismatch.")));
	}

	let user_id = user_id_from_username;

	// Only the failed-attempt axis: the caller already holds an access token,
	// but a stolen one must not become an unlimited password oracle.
	self.services
		.login_ratelimit
		.check_failed_login_rate_limit(&user_id)?;

	// First try local password hash verification
	let password_verified = self
		.services
		.users
		.password_hash(&user_id)
		.await
		.is_ok_and(|hash| verify_password(password, &hash).is_ok());

	// Only LDAP-origin accounts fall back to LDAP; others would trigger a
	// directory-wide search.
	#[cfg(feature = "ldap")]
	let password_verified = if !password_verified
		&& self.services.server.config.ldap.enable
		&& self
			.services
			.users
			.origin(&user_id)
			.await
			.is_ok_and(|origin| origin == "ldap")
		&& let Ok(dns) = self.services.users.search_ldap(&user_id).await
		&& let Some((user_dn, _is_admin)) = dns.first()
	{
		self.services
			.users
			.auth_ldap(user_dn, password)
			.await
			.is_ok()
	} else {
		password_verified
	};

	if !password_verified {
		self.services
			.login_ratelimit
			.record_failed_login(&user_id);
		uiaainfo.auth_error = Some(Box::new(StandardErrorBody {
			kind: ErrorKind::forbidden(),
			message: "Invalid username or password.".to_owned(),
		}));

		return Ok(ControlFlow::Break(false));
	}

	uiaainfo.completed.push(AuthType::Password);

	Ok(ControlFlow::Continue(()))
}

#[implement(Service)]
fn set_uiaa_request(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	session: &str,
	request: &CanonicalJsonValue,
) {
	let key = (user_id.to_owned(), device_id.to_owned(), session.to_owned());

	self.userdevicesessionid_uiaarequest
		.write()
		.expect("locked for writing")
		.insert(key, request.to_owned());
}

#[implement(Service)]
pub fn get_uiaa_request(
	&self,
	user_id: &UserId,
	device_id: Option<&DeviceId>,
	session: &str,
) -> Option<CanonicalJsonValue> {
	let device_id = device_id.unwrap_or_else(|| EMPTY.into());
	let key = (user_id.to_owned(), device_id.to_owned(), session.to_owned());

	self.userdevicesessionid_uiaarequest
		.read()
		.expect("locked for reading")
		.get(&key)
		.cloned()
}

#[implement(Service)]
pub fn update_uiaa_session(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	session: &str,
	uiaainfo: Option<&UiaaInfo>,
) {
	let key = (user_id, device_id, session);

	if let Some(uiaainfo) = uiaainfo {
		self.db
			.userdevicesessionid_uiaainfo
			.put(key, Json(uiaainfo));
	} else {
		self.db.userdevicesessionid_uiaainfo.del(key);
	}
}

#[implement(Service)]
async fn get_uiaa_session(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	session: &str,
) -> Result<UiaaInfo> {
	let key = (user_id, device_id, session);

	self.db
		.userdevicesessionid_uiaainfo
		.qry(&key)
		.await
		.deserialized()
		.map_err(|_| err!(Request(Forbidden("UIAA session does not exist."))))
}

#[implement(Service)]
pub async fn get_uiaa_session_by_session_id(
	&self,
	session_id: &str,
) -> Option<(OwnedUserId, OwnedDeviceId, UiaaInfo)> {
	// Iterate over keys only (fastest way without a secondary index)
	let stream = self
		.db
		.userdevicesessionid_uiaainfo
		.keys::<(OwnedUserId, OwnedDeviceId, String)>();

	pin_mut!(stream);
	while let Ok(Some((user_id, device_id, session))) = stream.try_next().await {
		if session == session_id {
			// Found the key, now fetch the actual UiaaInfo
			if let Ok(uiaainfo) = self
				.get_uiaa_session(&user_id, &device_id, session_id)
				.await
			{
				return Some((user_id, device_id, uiaainfo));
			}
		}
	}

	None
}
