use std::fmt::Write;

use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use futures::FutureExt;
use register::RegistrationKind;
use ruma::{
	UserId,
	api::client::{
		account::{
			check_registration_token_validity, get_username_availability,
			register::{self, LoginType},
		},
		uiaa::{AuthFlow, AuthType, UiaaInfo},
	},
	events::GlobalAccountDataEventType,
	push,
};
use tuwunel_core::{Err, Error, Result, debug_info, error, info, is_equal_to, utils, warn};
use tuwunel_service::users::device::generate_refresh_token;

use super::{DEVICE_ID_LENGTH, SESSION_ID_LENGTH};
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

	if let Some(ref info) = body.appservice_info {
		if !info.is_user_match(&user_id) {
			return Err!(Request(Exclusive("Username is not in an appservice namespace.")));
		}
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
#[allow(clippy::doc_markdown)]
#[tracing::instrument(skip_all, fields(%client), name = "register")]
pub(crate) async fn register_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<register::v3::Request>,
) -> Result<register::v3::Response> {
	let is_guest = body.kind == RegistrationKind::Guest;
	let emergency_mode_enabled = services.config.emergency_password.is_some();

	if !services.config.allow_registration && body.appservice_info.is_none() {
		match (body.username.as_ref(), body.initial_device_display_name.as_ref()) {
			| (Some(username), Some(device_display_name)) => {
				info!(
					%is_guest,
					user = %username,
					device_name = %device_display_name,
					"Rejecting registration attempt as registration is disabled"
				);
			},
			| (Some(username), _) => {
				info!(
					%is_guest,
					user = %username,
					"Rejecting registration attempt as registration is disabled"
				);
			},
			| (_, Some(device_display_name)) => {
				info!(
					%is_guest,
					device_name = %device_display_name,
					"Rejecting registration attempt as registration is disabled"
				);
			},
			| (None, _) => {
				info!(
					%is_guest,
					"Rejecting registration attempt as registration is disabled"
				);
			},
		}

		return Err!(Request(Forbidden("Registration has been disabled.")));
	}

	if is_guest
		&& (!services.config.allow_guest_registration
			|| (services.config.allow_registration
				&& services.globals.registration_token.is_some()))
	{
		info!(
			"Guest registration disabled / registration enabled with token configured, \
			 rejecting guest registration attempt, initial device name: \"{}\"",
			body.initial_device_display_name
				.as_deref()
				.unwrap_or("")
		);
		return Err!(Request(GuestAccessForbidden("Guest registration is disabled.")));
	}

	// forbid guests from registering if there is not a real admin user yet. give
	// generic user error.
	if is_guest && services.users.count().await < 2 {
		warn!(
			"Guest account attempted to register before a real admin user has been registered, \
			 rejecting registration. Guest's initial device name: \"{}\"",
			body.initial_device_display_name
				.as_deref()
				.unwrap_or("")
		);
		return Err!(Request(Forbidden("Registration is temporarily disabled.")));
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
	let skip_auth = if services.globals.registration_token.is_some() {
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

	// Create user
	services
		.users
		.create(&user_id, password, None)
		.await?;

	// Default to pretty displayname
	let mut displayname = user_id.localpart().to_owned();

	// If `new_user_displayname_suffix` is set, registration will push whatever
	// content is set to the user's display name with a space before it
	if !services
		.config
		.new_user_displayname_suffix
		.is_empty()
		&& body.appservice_info.is_none()
	{
		write!(displayname, " {}", services.server.config.new_user_displayname_suffix)?;
	}

	services
		.users
		.set_displayname(&user_id, Some(displayname.clone()));

	// Initial account data
	services
		.account_data
		.update(
			None,
			&user_id,
			GlobalAccountDataEventType::PushRules
				.to_string()
				.into(),
			&serde_json::to_value(ruma::events::push_rules::PushRulesEvent {
				content: ruma::events::push_rules::PushRulesEventContent {
					global: push::Ruleset::server_default(&user_id),
				},
			})?,
		)
		.await?;

	if (!is_guest && body.inhibit_login)
		|| body
			.appservice_info
			.as_ref()
			.is_some_and(|appservice| appservice.registration.device_management)
	{
		return Ok(register::v3::Response {
			access_token: None,
			user_id,
			device_id: None,
			refresh_token: None,
			expires_in: None,
		});
	}

	// Generate new device id if the user didn't specify one
	let device_id = if is_guest { None } else { body.device_id.clone() }
		.unwrap_or_else(|| utils::random_string(DEVICE_ID_LENGTH).into());

	// Generate new token for the device
	let (access_token, expires_in) = services
		.users
		.generate_access_token(body.body.refresh_token);

	// Generate a new refresh_token if requested by client
	let refresh_token = expires_in.is_some().then(generate_refresh_token);

	// Create device for this account
	services
		.users
		.create_device(
			&user_id,
			&device_id,
			(Some(&access_token), expires_in),
			refresh_token.as_deref(),
			body.initial_device_display_name.clone(),
			Some(client.to_string()),
		)
		.await?;

	debug_info!(%user_id, %device_id, "User account was created");

	let device_display_name = body
		.initial_device_display_name
		.as_deref()
		.unwrap_or("");

	// log in conduit admin channel if a non-guest user registered
	if body.appservice_info.is_none() && !is_guest {
		if !device_display_name.is_empty() {
			let notice = format!(
				"New user \"{user_id}\" registered on this server from IP {client} and device \
				 display name \"{device_display_name}\""
			);

			info!("{notice}");
			if services.server.config.admin_room_notices {
				services.admin.notice(&notice).await;
			}
		} else {
			let notice = format!("New user \"{user_id}\" registered on this server.");

			info!("{notice}");
			if services.server.config.admin_room_notices {
				services.admin.notice(&notice).await;
			}
		}
	}

	// log in conduit admin channel if a guest registered
	if body.appservice_info.is_none() && is_guest && services.config.log_guest_registrations {
		debug_info!("New guest user \"{user_id}\" registered on this server.");

		if !device_display_name.is_empty() {
			if services.server.config.admin_room_notices {
				services
					.admin
					.notice(&format!(
						"Guest user \"{user_id}\" with device display name \
						 \"{device_display_name}\" registered on this server from IP {client}"
					))
					.await;
			}
		} else {
			#[allow(clippy::collapsible_else_if)]
			if services.server.config.admin_room_notices {
				services
					.admin
					.notice(&format!(
						"Guest user \"{user_id}\" with no device display name registered on \
						 this server from IP {client}",
					))
					.await;
			}
		}
	}

	// If this is the first real user, grant them admin privileges except for guest
	// users
	// Note: the server user is generated first
	if !is_guest
		&& services.config.grant_admin_to_first_user
		&& let Ok(admin_room) = services.admin.get_admin_room().await
		&& services
			.state_cache
			.room_joined_count(&admin_room)
			.await
			.is_ok_and(is_equal_to!(1))
	{
		services
			.admin
			.make_user_admin(&user_id)
			.boxed()
			.await?;
		warn!("Granting {user_id} admin privileges as the first user");
	}

	if body.appservice_info.is_none()
		&& !services.server.config.auto_join_rooms.is_empty()
		&& (services.config.allow_guests_auto_join_rooms || !is_guest)
	{
		for room in &services.server.config.auto_join_rooms {
			let Ok(room_id) = services.alias.maybe_resolve(room).await else {
				error!(
					"Failed to resolve room alias to room ID when attempting to auto join \
					 {room}, skipping"
				);
				continue;
			};

			if !services
				.state_cache
				.server_in_room(services.globals.server_name(), &room_id)
				.await
			{
				warn!(
					"Skipping room {room} to automatically join as we have never joined before."
				);
				continue;
			}

			if let Some(room_server_name) = room.server_name() {
				let state_lock = services.state.mutex.lock(&room_id).await;

				match services
					.membership
					.join(
						&user_id,
						&room_id,
						Some("Automatically joining this room upon registration".to_owned()),
						&[services.globals.server_name().to_owned(), room_server_name.to_owned()],
						&body.appservice_info,
						&state_lock,
					)
					.boxed()
					.await
				{
					| Err(e) => {
						// don't return this error so we don't fail registrations
						error!(
							"Failed to automatically join room {room} for user {user_id}: {e}"
						);
					},
					| _ => {
						info!("Automatically joined room {room} for user {user_id}");
					},
				}

				drop(state_lock);
			}
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
	let Some(reg_token) = services.globals.registration_token.clone() else {
		return Err!(Request(Forbidden("Server does not allow token registration")));
	};

	Ok(check_registration_token_validity::v1::Response { valid: reg_token == body.token })
}
