use std::fmt::Write;

use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use ruma::{
	UserId,
	api::client::{
		account::{
			check_registration_token_validity, get_username_availability,
			register::{self, LoginType, RegistrationKind},
		},
		uiaa::{AuthFlow, AuthType, UiaaInfo},
	},
};
use tuwunel_core::{Err, Error, Result, debug_info, debug_warn, info, utils};
use tuwunel_service::users::{Register, device::generate_refresh_token};

use super::SESSION_ID_LENGTH;
use crate::Ruma;

const RANDOM_USER_ID_LENGTH: usize = 10;

/// # `GET /_matrix/client/v3/register/available`
///
/// Checks if a username is valid and available on this server.
///
/// Conditions for returning true:
/// - The user id is not historical
/// - The server name of the user id matches this server
/// - No user or appservice on this server already claimed this username
///
/// Note: This will not reserve the username, so the username might become
/// invalid when trying to register
#[tracing::instrument(skip_all, fields(%client), name = "register_available")]
pub(crate) async fn get_register_available_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_username_availability::v3::Request>,
) -> Result<get_username_availability::v3::Response> {
	// workaround for https://github.com/matrix-org/matrix-appservice-irc/issues/1780 due to inactivity of fixing the issue
	let is_matrix_appservice_irc = body
		.appservice_info
		.as_ref()
		.is_some_and(|appservice| {
			let id = &appservice.registration.id;
			id == "irc"
				|| id.contains("matrix-appservice-irc")
				|| id.contains("matrix_appservice_irc")
		});

	if services
		.config
		.forbidden_usernames
		.is_match(&body.username)
	{
		return Err!(Request(Forbidden("Username is forbidden")));
	}

	// don't force the username lowercase if it's from matrix-appservice-irc
	let body_username = if is_matrix_appservice_irc {
		body.username.clone()
	} else {
		body.username.to_lowercase()
	};

	// Validate user id
	let user_id =
		match UserId::parse_with_server_name(&body_username, services.globals.server_name()) {
			| Ok(user_id) => {
				if let Err(e) = user_id.validate_strict() {
					// unless the username is from the broken matrix appservice IRC bridge, we
					// should follow synapse's behaviour on not allowing things like spaces
					// and UTF-8 characters in usernames
					if !is_matrix_appservice_irc {
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

	// Check if username is creative enough
	if services.users.exists(&user_id).await {
		return Err!(Request(UserInUse("User ID is not available.")));
	}

	if let Some(ref info) = body.appservice_info
		&& !info.is_user_match(&user_id)
	{
		return Err!(Request(Exclusive("Username is not in an appservice namespace.")));
	}

	if services
		.appservice
		.is_exclusive_user_id(&user_id)
		.await
	{
		return Err!(Request(Exclusive("Username is reserved by an appservice.")));
	}

	Ok(get_username_availability::v3::Response { available: true })
}

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
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<register::v3::Request>,
) -> Result<register::v3::Response> {
	let is_guest = body.kind == RegistrationKind::Guest;
	let emergency_mode_enabled = services.config.emergency_password.is_some();

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

	let user_id = match (body.username.as_ref(), is_guest) {
		| (Some(username), false) => {
			// workaround for https://github.com/matrix-org/matrix-appservice-irc/issues/1780 due to inactivity of fixing the issue
			let is_matrix_appservice_irc =
				body.appservice_info
					.as_ref()
					.is_some_and(|appservice| {
						appservice.registration.id == "irc"
							|| appservice
								.registration
								.id
								.contains("matrix-appservice-irc")
							|| appservice
								.registration
								.id
								.contains("matrix_appservice_irc")
					});

			if services
				.config
				.forbidden_usernames
				.is_match(username)
				&& !emergency_mode_enabled
			{
				return Err!(Request(Forbidden("Username is forbidden")));
			}

			// don't force the username lowercase if it's from matrix-appservice-irc
			let body_username = if is_matrix_appservice_irc {
				username.clone()
			} else {
				username.to_lowercase()
			};

			let proposed_user_id = match UserId::parse_with_server_name(
				&body_username,
				services.globals.server_name(),
			) {
				| Ok(user_id) => {
					if let Err(e) = user_id.validate_strict() {
						// unless the username is from the broken matrix appservice IRC bridge, or
						// we are in emergency mode, we should follow synapse's behaviour on
						// not allowing things like spaces and UTF-8 characters in usernames
						if !is_matrix_appservice_irc && !emergency_mode_enabled {
							return Err!(Request(InvalidUsername(debug_warn!(
								"Username {body_username} contains disallowed characters or \
								 spaces: {e}"
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

			proposed_user_id
		},
		| _ => loop {
			let proposed_user_id = UserId::parse_with_server_name(
				utils::random_string(RANDOM_USER_ID_LENGTH).to_lowercase(),
				services.globals.server_name(),
			)
			.unwrap();
			if !services.users.exists(&proposed_user_id).await {
				break proposed_user_id;
			}
		},
	};

	if body.body.login_type == Some(LoginType::ApplicationService) {
		match body.appservice_info {
			| Some(ref info) =>
				if !info.is_user_match(&user_id) && !emergency_mode_enabled {
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
		.is_exclusive_user_id(&user_id)
		.await && !emergency_mode_enabled
	{
		return Err!(Request(Exclusive("Username is reserved by an appservice.")));
	}

	// UIAA
	let mut uiaainfo;
	let skip_auth = if services.registration_tokens.is_enabled().await && !is_guest {
		// Registration token required
		uiaainfo = UiaaInfo {
			flows: vec![AuthFlow {
				stages: vec![AuthType::RegistrationToken],
			}],
			completed: Vec::new(),
			params: Default::default(),
			session: None,
			auth_error: None,
		};

		body.appservice_info.is_some()
	} else {
		// No registration token necessary, but clients must still go through the flow
		uiaainfo = UiaaInfo {
			flows: vec![AuthFlow { stages: vec![AuthType::Dummy] }],
			completed: Vec::new(),
			params: Default::default(),
			session: None,
			auth_error: None,
		};

		body.appservice_info.is_some() || is_guest
	};

	if !skip_auth {
		match &body.auth {
			| Some(auth) => {
				let (worked, uiaainfo) = services
					.uiaa
					.try_auth(
						&UserId::parse_with_server_name("", services.globals.server_name())
							.unwrap(),
						"".into(),
						auth,
						&uiaainfo,
					)
					.await?;
				if !worked {
					return Err(Error::Uiaa(uiaainfo));
				}
				// Success!
			},
			| _ => match body.json_body {
				| Some(ref json) => {
					uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
					services.uiaa.create(
						&UserId::parse_with_server_name("", services.globals.server_name())
							.unwrap(),
						"".into(),
						&uiaainfo,
						json,
					);
					return Err(Error::Uiaa(uiaainfo));
				},
				| _ => {
					return Err!(Request(NotJson("JSON body is not valid")));
				},
			},
		}
	}

	let password = if is_guest { None } else { body.password.as_deref() };

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password,
			appservice_info: body.appservice_info.as_ref(),
			is_guest,
			grant_first_user_admin: true,
			..Default::default()
		})
		.await?;

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
			Some(client.to_string()),
		)
		.await?;

	debug_info!(%user_id, %device_id, "User account was created");

	if body.appservice_info.is_none() && (!is_guest || services.config.log_guest_registrations) {
		let mut notice = String::from(if is_guest { "New guest user" } else { "New user" });

		write!(notice, " registered on this server from IP {client}")?;

		if let Some(device_name) = body.initial_device_display_name.as_deref() {
			write!(notice, " with device name {device_name}")?;
		}

		if !is_guest {
			info!("{notice}");
		} else {
			debug_info!("{notice}");
		}

		if services.server.config.admin_room_notices {
			services.admin.notice(&notice).await;
		}
	}

	Ok(register::v3::Response {
		user_id,
		device_id: Some(device_id),
		access_token: Some(access_token),
		refresh_token,
		expires_in,
	})
}

/// # `GET /_matrix/client/v1/register/m.login.registration_token/validity`
///
/// Checks if the provided registration token is valid at the time of checking
///
/// Currently does not have any ratelimiting, and this isn't very practical as
/// there is only one registration token allowed.
pub(crate) async fn check_registration_token_validity(
	State(services): State<crate::State>,
	body: Ruma<check_registration_token_validity::v1::Request>,
) -> Result<check_registration_token_validity::v1::Response> {
	if !services.registration_tokens.is_enabled().await {
		return Err!(Request(Forbidden("Server does not allow token registration")));
	}

	let valid = services
		.registration_tokens
		.is_token_valid(&body.token)
		.await
		.is_ok();

	Ok(check_registration_token_validity::v1::Response { valid })
}
