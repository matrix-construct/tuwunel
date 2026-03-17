use ruma::{
	CanonicalJsonValue, OwnedUserId,
	api::{
		IncomingRequest,
		client::uiaa::{AuthData, AuthFlow, AuthType, Jwt, UiaaInfo},
	},
};
use tuwunel_core::{
	Err, Error, Result, err, utils,
	utils::{
		OptionExt,
		future::{OptionFutureExt, TryExtExt},
	},
};
use tuwunel_service::{Services, uiaa::SESSION_ID_LENGTH};

use crate::{Ruma, client::jwt};

pub(crate) async fn auth_uiaa<T>(services: &Services, body: &Ruma<T>) -> Result<OwnedUserId>
where
	T: IncomingRequest + Send + Sync,
{
	let sender_device = body.sender_device()?;
	let sender_user = body.sender_user.as_deref();

	let password_flow = [AuthType::Password];
	let has_password = sender_user
		.map_async(|sender_user| {
			services
				.users
				.has_password(sender_user)
				.unwrap_or(false)
		})
		.unwrap_or(false)
		.await || (cfg!(feature = "ldap") && services.config.ldap.enable);

	//TODO: UIAA for SSO.
	let sso_flow = [AuthType::Sso];
	let has_sso = false;
	let _has_sso = sender_user
		.map_async(|sender_user| {
			services
				.oauth
				.sessions
				.exists_for_user(sender_user)
		})
		.unwrap_or(false)
		.await;

	//NOTE: Not implemented as a fallback/web for now.
	let has_jwt = false;
	let jwt_flow = [AuthType::Jwt];

	let mut uiaainfo = UiaaInfo {
		flows: has_password
			.then_some(password_flow)
			.into_iter()
			.chain(has_sso.then_some(sso_flow))
			.chain(has_jwt.then_some(jwt_flow))
			.map(Vec::from)
			.map(AuthFlow::new)
			.collect(),

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
		| Some(AuthData::Jwt(Jwt { ref token, .. })) => {
			let sender_user = jwt::validate_user(services, token)?;
			if !services.users.exists(&sender_user).await {
				return Err!(Request(NotFound("User {sender_user} is not registered.")));
			}

			// Success!
			Ok(sender_user)
		},
		| Some(ref auth) => {
			let sender_user = body
				.sender_user
				.as_deref()
				.ok_or_else(|| err!(Request(MissingToken("Missing access token."))))?;

			let (worked, uiaainfo) = services
				.uiaa
				.try_auth(sender_user, sender_device, auth, &uiaainfo)
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

				uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
				services
					.uiaa
					.create(sender_user, sender_device, &uiaainfo, json);

				Err(Error::Uiaa(uiaainfo))
			},
			| _ => Err!(Request(NotJson("JSON body is not valid"))),
		},
	}
}
