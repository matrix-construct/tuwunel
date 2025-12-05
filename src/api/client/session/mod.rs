mod appservice;
pub(crate) mod jwt;
mod ldap;
mod logout;
mod password;
mod refresh;
mod token;

use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use ruma::api::client::session::{
	get_login_types::{
		self,
		v3::{
			ApplicationServiceLoginType, JwtLoginType, LoginType, PasswordLoginType,
			TokenLoginType,
		},
	},
	login::{
		self,
		v3::{DiscoveryInfo, HomeserverInfo, LoginInfo},
	},
};
use tuwunel_core::{Err, Result, info, utils::stream::ReadyExt};
use tuwunel_service::users::device::generate_refresh_token;

use self::{ldap::ldap_login, password::password_login};
pub(crate) use self::{
	logout::{logout_all_route, logout_route},
	refresh::refresh_token_route,
	token::login_token_route,
};
use super::TOKEN_LENGTH;
use crate::Ruma;

/// # `GET /_matrix/client/v3/login`
///
/// Get the supported login types of this server. One of these should be used as
/// the `type` field when logging in.
#[tracing::instrument(skip_all, fields(%client), name = "login")]
pub(crate) async fn get_login_types_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	_body: Ruma<get_login_types::v3::Request>,
) -> Result<get_login_types::v3::Response> {
	Ok(get_login_types::v3::Response::new(vec![
		LoginType::Password(PasswordLoginType::default()),
		LoginType::ApplicationService(ApplicationServiceLoginType::default()),
		LoginType::Jwt(JwtLoginType::default()),
		LoginType::Token(TokenLoginType {
			get_login_token: services.config.login_via_existing_session,
		}),
	]))
}

/// # `POST /_matrix/client/v3/login`
///
/// Authenticates the user and returns an access token it can use in subsequent
/// requests.
///
/// - The user needs to authenticate using their password (or if enabled using a
///   json web token)
/// - If `device_id` is known: invalidates old access token of that device
/// - If `device_id` is unknown: creates a new device
/// - Returns access token that is associated with the user and device
///
/// Note: You can use [`GET
/// /_matrix/client/r0/login`](fn.get_supported_versions_route.html) to see
/// supported login types.
#[tracing::instrument(name = "login", skip_all, fields(%client, ?body.login_info))]
pub(crate) async fn login_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<login::v3::Request>,
) -> Result<login::v3::Response> {
	// Validate login method
	let user_id = match &body.login_info {
		| LoginInfo::Password(info) => password::handle_login(&services, &body, info).await?,
		| LoginInfo::Token(info) => token::handle_login(&services, &body, info).await?,
		| LoginInfo::Jwt(info) => jwt::handle_login(&services, &body, info).await?,
		| LoginInfo::ApplicationService(info) =>
			appservice::handle_login(&services, &body, info).await?,
		| _ => {
			return Err!(Request(Unknown(debug_warn!(
				?body.login_info,
				?body.json_body,
				"Invalid or unsupported login type",
			))));
		},
	};

	// Generate a new token for the device
	let (access_token, expires_in) = services
		.users
		.generate_access_token(body.body.refresh_token);

	// Generate a new refresh_token if requested by client
	let refresh_token = expires_in.is_some().then(generate_refresh_token);

	// Determine if device_id was provided and exists in the db for this user
	let device_id = if let Some(device_id) = &body.device_id
		&& services
			.users
			.all_device_ids(&user_id)
			.ready_any(|v| v == device_id)
			.await
	{
		services
			.users
			.set_access_token(
				&user_id,
				device_id,
				&access_token,
				expires_in,
				refresh_token.as_deref(),
			)
			.await?;

		device_id.clone()
	} else {
		services
			.users
			.create_device(
				&user_id,
				body.device_id.as_deref(),
				(Some(&access_token), expires_in),
				refresh_token.as_deref(),
				body.initial_device_display_name.as_deref(),
				Some(client.to_string()),
			)
			.await?
	};

	info!("{user_id} logged in");

	let home_server = services.server.name.clone().into();

	// send client well-known if specified so the client knows to reconfigure itself
	let well_known: Option<DiscoveryInfo> = services
		.config
		.well_known
		.client
		.as_ref()
		.map(ToString::to_string)
		.map(HomeserverInfo::new)
		.map(DiscoveryInfo::new);

	#[allow(deprecated)]
	Ok(login::v3::Response {
		user_id,
		access_token,
		device_id,
		home_server,
		well_known,
		expires_in,
		refresh_token,
	})
}
