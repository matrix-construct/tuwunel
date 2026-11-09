use std::borrow::Cow;

use ruma::{
	CanonicalJsonValue, DeviceId, OwnedUserId, UserId,
	api::{
		IncomingRequest,
		client::uiaa::{AuthData, AuthFlow, AuthType, UiaaInfo},
	},
};
use serde_json::{json, value::to_raw_value};
use tuwunel_core::{
	Err, Error, Result, err, is_equal_to, utils,
	utils::{
		BoolExt, OptionExt,
		future::{OptionFutureExt, TryExtExt},
	},
};
use tuwunel_service::{Services, uiaa::SESSION_ID_LENGTH};

#[cfg(feature = "jwt_login")]
use super::jwt::validate_user;
use crate::Ruma;

/// Authenticates a user through the request's UIAA flow.
///
/// The configured authentication stages determine whether the flow is complete.
pub async fn auth_uiaa<T>(services: &Services, body: &Ruma<T>) -> Result<OwnedUserId>
where
	T: IncomingRequest + Send + Sync,
{
	let sender_user = body.sender_user.as_deref();

	let password_flow = [AuthType::Password];
	let user_origin = sender_user
		.map_async(|sender_user| services.users.origin(sender_user).ok())
		.unwrap_or(None)
		.await;

	let ldap_origin = cfg!(feature = "ldap")
		&& services.config.ldap.enable
		&& user_origin.as_deref() == Some("ldap");

	let has_password = ldap_origin
		|| sender_user
			.map_async(|sender_user| {
				services
					.users
					.has_password(sender_user)
					.unwrap_or(false)
			})
			.unwrap_or(false)
			.await;

	let sso_flow = [AuthType::Sso];
	let bound_idp = sender_user
		.map_async(|sender_user| {
			select_idp(
				services,
				sender_user,
				body.sender_device.as_deref(),
				user_origin.as_deref(),
			)
		})
		.await
		.flatten();

	let has_sso = bound_idp.is_some();

	let jwt_flow = [AuthType::Jwt];
	let has_jwt = cfg!(feature = "jwt_login") && services.config.jwt.enable;

	let mut uiaainfo = UiaaInfo {
		flows: has_password
			.then_some(password_flow)
			.into_iter()
			.chain(has_sso.then_some(sso_flow))
			.chain(has_jwt.then_some(jwt_flow))
			.map(Vec::from)
			.map(AuthFlow::new)
			.collect(),

		params: to_raw_value(&json!({})).ok(),
		..Default::default()
	};

	match body
		.json_body
		.as_ref()
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|body| body.get("auth"))
		.cloned()
		.map(CanonicalJsonValue::into)
		.map(serde_json::from_value)
		.transpose()?
	{
		#[cfg(feature = "jwt_login")]
		| Some(AuthData::Jwt(ref jwt)) => {
			let sender_user = validate_user(services, &jwt.token)?;

			if !services.users.exists(&sender_user).await {
				return Err!(Request(NotFound("User {sender_user} is not registered.")));
			}

			services
				.users
				.deactivated_check(&sender_user)
				.await?;

			// Success!
			Ok(sender_user)
		},
		#[cfg(not(feature = "jwt_login"))]
		| Some(AuthData::Jwt(_)) => Err!(Request(Unauthorized("JWT login is not enabled."))),
		| Some(ref auth) => {
			let sender_user = body
				.sender_user
				.as_deref()
				.ok_or_else(|| err!(Request(MissingToken("Missing access token."))))?;

			let sender_device = body.sender_device()?;
			let (worked, uiaainfo) = services
				.uiaa
				.try_auth(sender_user, sender_device, auth, &uiaainfo, body.rate_limit_ip.ok())
				.await?;

			if !worked {
				return Err(Error::Uiaa(uiaainfo));
			}

			// Success!
			Ok(sender_user.to_owned())
		},
		| _ => match body.json_body {
			| Some(ref json) => {
				let sender_user = body
					.sender_user
					.as_deref()
					.ok_or_else(|| err!(Request(MissingToken("Missing access token."))))?;

				let sender_device = body.sender_device()?;
				uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));

				// The SSO fallback page routes re-authentication to the provider bound here.
				if let Some(ref idp) = bound_idp {
					uiaainfo.params = to_raw_value(&json!({
						"m.login.sso": {
							"identity_providers": [{"id": idp}]
						}
					}))
					.ok();
				}

				services
					.uiaa
					.create(sender_user, sender_device, &uiaainfo, json);

				Err(Error::Uiaa(uiaainfo))
			},
			| _ => Err!(Request(NotJson("JSON body is not valid"))),
		},
	}
}

/// Choose the identity provider to bind to the UIAA session's SSO stage.
///
/// A device the OIDC token endpoint tagged binds its own provider. Otherwise an
/// `sso` account on a single-provider server binds that provider. Failing both,
/// it binds a configured provider that one of the account's sessions links,
/// preferring the default. The SSO callback completes the stage only for an
/// identity that resolves to the UIAA session's owner.
async fn select_idp<'a>(
	services: &'a Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	user_origin: Option<&str>,
) -> Option<Cow<'a, str>> {
	let tagged = sender_device
		.map_async(|device_id| {
			services
				.users
				.get_oidc_device_idp(sender_user, device_id)
		})
		.await
		.flatten()
		.map(Cow::Owned)
		.or_else(|| {
			let single = user_origin.is_some_and(is_equal_to!("sso"))
				&& services.config.identity_provider.len() == 1;

			single
				.and_then(|| services.oauth.providers.find_default_config())
				.map(|provider| Cow::Borrowed(provider.id()))
		});

	tagged
		.is_none()
		.then_async(|| {
			services
				.oauth
				.sessions
				.linked_provider(sender_user)
		})
		.await
		.flatten()
		.map(Cow::Borrowed)
		.or(tagged)
}
