use ruma::{
	CanonicalJsonValue, OwnedUserId,
	api::{
		IncomingRequest,
		client::uiaa::{AuthData, AuthFlow, AuthType, Jwt, UiaaInfo},
	},
};
use serde_json::{json, value::to_raw_value};
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
	let sender_user = body.sender_user.as_deref();

	if let Some(sender_user) = sender_user {
		services
			.users
			.maybe_repair_legacy_sso_origin(sender_user)
			.await;
	}

	let sender_uses_sso = if let Some(sender_user) = sender_user {
		services
			.users
			.origin(sender_user)
			.await
			.is_ok_and(|origin| origin == "sso")
	} else {
		false
	};

	let password_flow = [AuthType::Password];
	let has_password = !sender_uses_sso
		&& (sender_user
			.map_async(|sender_user| {
				services
					.users
					.has_password(sender_user)
					.unwrap_or(false)
			})
			.unwrap_or(false)
			.await || (cfg!(feature = "ldap") && services.config.ldap.enable));

	// Determine the exact IdP to bind to the UIAA session.
	//
	// The correct binding comes from the device that made this request, not
	// from a heuristic scan of all user sessions.  Rules:
	//
	//  1. Preferred: the device is tagged with an idp_id from when it was created
	//     via the OIDC token endpoint → use that idp_id directly. This is exact and
	//     correct even on multi-provider servers.
	//  2. Fallback: the device has no idp tag (pre-dates the idp_id field or was
	//     created through a legacy path) but origin=="sso" and only one provider is
	//     configured → routing is still unambiguous.
	//  3. Otherwise: cannot determine provider → do NOT advertise m.login.sso.
	let sso_flow = [AuthType::Sso];
	let device_bound_idp: Option<String> = if sender_uses_sso {
		sender_user
			.map_async(async |sender_user| {
				body.sender_device
					.as_deref()
					.map_async(async |device_id| {
						services
							.users
							.get_oidc_device_idp(sender_user, device_id)
							.await
							.filter(|s| !s.is_empty())
					})
					.await
					.flatten()
			})
			.await
			.flatten()
	} else {
		None
	};
	let bound_idp = select_bound_sso_idp(
		sender_uses_sso,
		device_bound_idp,
		services.config.identity_provider.len(),
		services.oauth.providers.get_default_id(),
	);

	let has_sso = bound_idp.is_some();

	// NOTE: JWT fallback/web is not implemented for UIAA.
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
		| Some(AuthData::Jwt(Jwt { ref token, .. })) => {
			if sender_uses_sso {
				return Err!(Request(
					Forbidden("JWT UIAA is not allowed for SSO-origin users.",)
				));
			}

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

			let sender_device = body.sender_device()?;
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

				let sender_device = body.sender_device()?;
				uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));

				// Bind the exact IdP determined above into the UIAA session so
				// the SSO fallback page can route re-authentication to the
				// correct provider without any further heuristic lookups.
				if let Some(ref idp) = bound_idp {
					bind_sso_identity_provider(&mut uiaainfo, idp);
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

fn select_bound_sso_idp(
	sender_uses_sso: bool,
	device_idp: Option<String>,
	identity_provider_count: usize,
	default_idp: Option<String>,
) -> Option<String> {
	if !sender_uses_sso {
		return None;
	}

	device_idp
		.filter(|idp| !idp.is_empty())
		.or_else(|| {
			(identity_provider_count == 1)
				.then_some(default_idp)
				.flatten()
		})
}

fn bind_sso_identity_provider(uiaainfo: &mut UiaaInfo, idp: &str) {
	uiaainfo.params = to_raw_value(&json!({
		"m.login.sso": {
			"identity_providers": [{"id": idp}]
		}
	}))
	.ok();
}

#[cfg(test)]
mod tests {
	use serde_json::Value as JsonValue;

	use super::*;

	#[test]
	fn sso_idp_binding_prefers_request_device_idp() {
		let selected =
			select_bound_sso_idp(true, Some("apple".to_owned()), 1, Some("google".to_owned()));

		assert_eq!(selected.as_deref(), Some("apple"));
	}

	#[test]
	fn sso_idp_binding_falls_back_to_single_default_provider() {
		let selected = select_bound_sso_idp(true, None, 1, Some("default".to_owned()));

		assert_eq!(selected.as_deref(), Some("default"));
	}

	#[test]
	fn sso_idp_binding_rejects_ambiguous_multi_provider_fallback() {
		let selected = select_bound_sso_idp(true, None, 2, Some("default".to_owned()));

		assert_eq!(selected, None);
	}

	#[test]
	fn sso_idp_binding_is_not_available_for_password_origin_users() {
		let selected =
			select_bound_sso_idp(false, Some("apple".to_owned()), 1, Some("default".to_owned()));

		assert_eq!(selected, None);
	}

	#[test]
	fn bound_sso_idp_is_serialized_into_uiaa_params() {
		let mut uiaainfo = UiaaInfo::default();

		bind_sso_identity_provider(&mut uiaainfo, "apple");

		let params: JsonValue = serde_json::from_str(
			uiaainfo
				.params
				.as_ref()
				.expect("params should be populated")
				.get(),
		)
		.expect("valid params JSON");
		assert_eq!(
			params["m.login.sso"]["identity_providers"][0]["id"],
			JsonValue::String("apple".to_owned()),
		);
	}
}
