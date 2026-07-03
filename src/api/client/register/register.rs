use std::{fmt::Write, net::IpAddr};

use axum::extract::State;
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedUserId, UserId,
	api::client::{
		account::register::{self, LoginType, RegistrationKind},
		uiaa::{AuthFlow, AuthType, ThirdpartyIdCredentials, UiaaInfo},
	},
};
use serde_json::{json, value::to_raw_value};
use tuwunel_core::{Err, Error, Result, debug_info, debug_warn, info, utils, warn};
use tuwunel_service::users::{Register, device::generate_refresh_token};

use super::{SESSION_ID_LENGTH, is_matrix_appservice_irc};
use crate::{ClientIp, Ruma};

const RANDOM_USER_ID_LENGTH: usize = 10;

/// # `POST /_matrix/client/v3/register`
///
/// Register an account on this homeserver.
///
/// You can use [`GET
/// /_matrix/client/v3/register/available`](fn.get_register_available_route.
/// html) to check if the user id is valid and available.
///
/// - Only works if registration is enabled
/// - If type is guest: ignores all parameters except
///   initial_device_display_name
/// - If sender is not appservice: Requires UIAA (but we only use a dummy stage)
/// - If type is not guest and no username is given: Always fails after UIAA
///   check
/// - Creates a new account and populates it with default account data
/// - If `inhibit_login` is false: Creates a device and returns device id and
///   access_token
#[expect(clippy::doc_markdown)]
#[tracing::instrument(skip_all, fields(%client), name = "register")]
pub(crate) async fn register_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<register::v3::Request>,
) -> Result<register::v3::Response> {
	let is_guest = body.kind == RegistrationKind::Guest;
	let emergency_mode_enabled = services.config.emergency_password.is_some();

	gate_registration_allowed(services, &body, is_guest)?;

	// MSC4190: an appservice managing its own devices must register with
	// inhibit_login set; it cannot mint a login session via /register.
	if !body.inhibit_login
		&& body
			.appservice_info
			.as_ref()
			.is_some_and(|appservice| appservice.registration.device_management)
	{
		return Err!(Request(AppserviceLoginUnsupported(
			"Appservice has MSC4190 device management enabled; inhibit_login must be true."
		)));
	}

	let user_id =
		resolve_registration_user_id(services, &body, is_guest, emergency_mode_enabled).await?;

	check_appservice_namespace(services, &body, &user_id, emergency_mode_enabled).await?;

	let email_creds = enforce_uiaa(services, &body, is_guest).await?;

	let password = if is_guest { None } else { body.password.as_deref() };

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password,
			is_appservice: body.appservice_info.is_some(),
			is_guest,
			grant_first_user_admin: true,
			..Default::default()
		})
		.await?;

	record_accepted_terms(services, &user_id, &body, is_guest).await?;

	bind_registration_email(services, &user_id, email_creds.as_ref()).await?;

	if (!is_guest && body.inhibit_login)
		|| body
			.appservice_info
			.as_ref()
			.is_some_and(|appservice| appservice.registration.device_management)
	{
		return Ok(register::v3::Response {
			user_id,
			device_id: None,
			access_token: None,
			refresh_token: None,
			expires_in: None,
		});
	}

	let device_id = if is_guest { None } else { body.device_id.as_deref() };

	// Generate new token for the device
	let (access_token, expires_in) = services
		.users
		.generate_access_token(body.refresh_token);

	// Generate a new refresh_token if requested by client
	let refresh_token = expires_in.is_some().then(generate_refresh_token);

	// Create device for this account
	let device_id = services
		.users
		.create_device(
			&user_id,
			device_id,
			(Some(&access_token), expires_in),
			refresh_token.as_deref(),
			body.initial_device_display_name.as_deref(),
			Some(client),
		)
		.await?;

	debug_info!(%user_id, %device_id, "User account was created");

	if body.appservice_info.is_none() && (!is_guest || services.config.log_guest_registrations) {
		announce_new_user(services, &user_id, &body, is_guest, &client).await?;
	}

	Ok(register::v3::Response {
		user_id,
		device_id: Some(device_id),
		access_token: Some(access_token),
		refresh_token,
		expires_in,
	})
}

fn gate_registration_allowed(
	services: crate::State,
	body: &Ruma<register::v3::Request>,
	is_guest: bool,
) -> Result {
	let user = body.username.as_deref().unwrap_or("");
	let device_name = body
		.initial_device_display_name
		.as_deref()
		.unwrap_or("");

	if !services.config.allow_registration && body.appservice_info.is_none() {
		info!(
			%is_guest,
			%user,
			%device_name,
			"Rejecting registration attempt as registration is disabled"
		);

		return Err!(Request(Forbidden("Registration has been disabled.")));
	}

	if is_guest && !services.config.allow_guest_registration {
		debug_warn!(
			%device_name,
			"Guest registration disabled, rejecting guest registration attempt"
		);

		return Err!(Request(GuestAccessForbidden("Guest registration is disabled.")));
	}

	Ok(())
}

async fn resolve_registration_user_id(
	services: crate::State,
	body: &Ruma<register::v3::Request>,
	is_guest: bool,
	emergency_mode_enabled: bool,
) -> Result<OwnedUserId> {
	let (Some(username), false) = (body.username.as_ref(), is_guest) else {
		loop {
			let proposed_user_id = UserId::parse_with_server_name(
				utils::random_string(RANDOM_USER_ID_LENGTH).to_lowercase(),
				services.globals.server_name(),
			)?;

			if !services.users.exists(&proposed_user_id).await {
				return Ok(proposed_user_id);
			}
		}
	};

	let is_irc = is_matrix_appservice_irc(body.appservice_info.as_ref());

	if services
		.config
		.forbidden_usernames
		.is_match(username)
		&& !emergency_mode_enabled
	{
		return Err!(Request(Forbidden("Username is forbidden")));
	}

	// don't force the username lowercase if it's from matrix-appservice-irc
	let body_username = if is_irc {
		username.clone()
	} else {
		username.to_lowercase()
	};

	let proposed_user_id =
		match UserId::parse_with_server_name(&body_username, services.globals.server_name()) {
			| Ok(user_id) => {
				if let Err(e) = user_id.validate_strict() {
					// unless the username is from the broken matrix appservice IRC bridge, or
					// we are in emergency mode, we should follow synapse's behaviour on
					// not allowing things like spaces and UTF-8 characters in usernames
					if !is_irc && !emergency_mode_enabled {
						return Err!(Request(InvalidUsername(debug_warn!(
							"Username {body_username} contains disallowed characters or spaces: \
							 {e}"
						))));
					}
				}

				user_id
			},
			| Err(e) => {
				return Err!(Request(InvalidUsername(debug_warn!(
					"Username {body_username} is not valid: {e}"
				))));
			},
		};

	if services.users.exists(&proposed_user_id).await {
		return Err!(Request(UserInUse("User ID is not available.")));
	}

	Ok(proposed_user_id)
}

async fn check_appservice_namespace(
	services: crate::State,
	body: &Ruma<register::v3::Request>,
	user_id: &UserId,
	emergency_mode_enabled: bool,
) -> Result {
	if body.body.login_type == Some(LoginType::ApplicationService) {
		match body.appservice_info {
			| Some(ref info) =>
				if !info.is_user_match(user_id) && !emergency_mode_enabled {
					return Err!(Request(Exclusive(
						"Username is not in an appservice namespace."
					)));
				},
			| _ => {
				return Err!(Request(MissingToken("Missing appservice token.")));
			},
		}
	} else if services
		.appservice
		.is_exclusive_user_id(user_id)
		.await && !emergency_mode_enabled
	{
		return Err!(Request(Exclusive("Username is reserved by an appservice.")));
	}

	Ok(())
}

async fn enforce_uiaa(
	services: crate::State,
	body: &Ruma<register::v3::Request>,
	is_guest: bool,
) -> Result<Option<ThirdpartyIdCredentials>> {
	if body.appservice_info.is_some() || is_guest {
		return Ok(None);
	}

	let token_required = services.registration_tokens.is_enabled().await;
	let terms = services.config.login_terms_params();

	let smtp = &services.config.smtp;
	let email_required = smtp.connection_uri.is_some()
		&& (smtp.require_email_for_registration
			|| (token_required && smtp.require_email_for_token_registration));

	let stages: Vec<AuthType> = [
		token_required.then_some(AuthType::RegistrationToken),
		email_required.then_some(AuthType::EmailIdentity),
		terms.is_some().then_some(AuthType::Terms),
	]
	.into_iter()
	.flatten()
	.collect();

	// A dummy stage still forces the client through UIA when nothing else does.
	let stages = if stages.is_empty() {
		vec![AuthType::Dummy]
	} else {
		stages
	};

	let params = terms
		.as_ref()
		.map(|terms| to_raw_value(&json!({ "m.login.terms": terms })))
		.transpose()?;

	let mut uiaainfo = UiaaInfo {
		flows: vec![AuthFlow { stages }],
		completed: Vec::new(),
		params,
		session: None,
		auth_error: None,
	};

	let server_user = UserId::parse_with_server_name("", services.globals.server_name())?;

	match &body.auth {
		| Some(auth) => {
			let (worked, uiaainfo) = services
				.uiaa
				.try_auth(&server_user, "".into(), auth, &uiaainfo)
				.await?;

			if !worked {
				return Err(Error::Uiaa(uiaainfo));
			}

			// Recover the validated email creds the session retained, so the bind works
			// regardless of which stage completed the flow.
			let creds = email_required
				.then_some(uiaainfo.session.as_deref())
				.flatten()
				.and_then(|session| {
					services
						.uiaa
						.take_uiaa_threepid(&server_user, "".into(), session)
				});

			Ok(creds)
		},
		| _ => match body.json_body {
			| None => Err!(Request(NotJson("JSON body is not valid"))),
			| Some(ref json) => {
				uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
				services
					.uiaa
					.create(&server_user, "".into(), &uiaainfo, json);

				Err(Error::Uiaa(uiaainfo))
			},
		},
	}
}

async fn record_accepted_terms(
	services: crate::State,
	user_id: &UserId,
	body: &Ruma<register::v3::Request>,
	is_guest: bool,
) -> Result {
	if is_guest || body.appservice_info.is_some() {
		return Ok(());
	}

	let accepted: Vec<String> = services
		.config
		.registration_terms
		.values()
		.flat_map(|policy| policy.translations.values())
		.map(|translation| translation.url.to_string())
		.collect();

	if accepted.is_empty() {
		return Ok(());
	}

	let event_type = "m.accepted_terms";
	let event = json!({
		"type": event_type,
		"content": { "accepted": accepted },
	});

	services
		.account_data
		.update(None, user_id, event_type.into(), &event)
		.await
}

/// A stray `id_server` on the credentials is ignored and `id_access_token` is
/// never required.
async fn bind_registration_email(
	services: crate::State,
	user_id: &UserId,
	creds: Option<&ThirdpartyIdCredentials>,
) -> Result {
	if !services.sendmail.is_enabled() {
		return Ok(());
	}

	let Some(creds) = creds else {
		return Ok(());
	};

	if let Err(e) = try_bind_registration_email(services, user_id, creds).await {
		warn!(%user_id, "Skipping registration email binding: {e}");
	}

	Ok(())
}

async fn try_bind_registration_email(
	services: crate::State,
	user_id: &UserId,
	thirdparty_id_creds: &ThirdpartyIdCredentials,
) -> Result {
	let association = services
		.threepid
		.consume_validated(
			thirdparty_id_creds.sid.as_str(),
			thirdparty_id_creds.client_secret.as_str(),
		)
		.await?;

	if services
		.threepid
		.user_id_for_email(&association.address)
		.await?
		.is_some_and(|bound| bound != user_id)
	{
		warn!(%user_id, "Skipping registration email binding: address bound to another user");

		return Ok(());
	}

	let now = MilliSecondsSinceUnixEpoch::now();

	services
		.threepid
		.put_binding(user_id, &association.address, association.medium, now, now)
		.await;

	Ok(())
}

async fn announce_new_user(
	services: crate::State,
	user_id: &UserId,
	body: &Ruma<register::v3::Request>,
	is_guest: bool,
	client: &IpAddr,
) -> Result {
	let mut notice = String::from(if is_guest { "New guest user" } else { "New user" });

	write!(notice, " \"{user_id}\" registered on this server from IP {client}")?;

	if let Some(device_name) = body.initial_device_display_name.as_deref() {
		write!(notice, " with device name {device_name}")?;
	}

	if is_guest {
		debug_info!("{notice}");
	} else {
		info!("{notice}");
	}

	services.admin.notify(&notice).await;

	Ok(())
}
