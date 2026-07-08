mod native_apple;
mod uiaa;

use std::{borrow::Cow, collections::BTreeMap, net::IpAddr, time::Duration};

use axum::extract::State;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use futures::{FutureExt, TryFutureExt, future::try_join};
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use ruma::{
	Mxc, OwnedMxcUri, OwnedUserId, ServerName, UserId,
	api::client::{
		session::{SsoRedirectAction, sso_callback, sso_login, sso_login_with_provider},
		uiaa::AuthType,
	},
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Result, at,
	config::IdentityProvider,
	debug::INFO_SPAN_LEVEL,
	debug_info, debug_warn, err, info, is_not_equal_to,
	itertools::Itertools,
	utils,
	utils::{
		OptionExt,
		content_disposition::make_content_disposition,
		hash::sha256,
		result::{FlatOk, LogErr},
		string::{EMPTY, truncate_deterministic},
		timepoint_from_now, timepoint_has_passed,
	},
	warn,
};
use tuwunel_service::{
	Services,
	client::read_response_capped,
	media::MXC_LENGTH,
	oauth::{
		CODE_VERIFIER_LENGTH, Provider, SESSION_ID_LENGTH, Session, TokenResponse, UserInfo,
		unique_id_sub,
	},
	users::{PASSWORD_SENTINEL, Register},
};
use url::Url;

pub(crate) use self::{native_apple::native_apple_login_route, uiaa::sso_fallback_route};
use super::TOKEN_LENGTH;
use crate::{ClientIp, Ruma};

/// Grant phase query string.
#[derive(Debug, Serialize)]
struct GrantQuery<'a> {
	client_id: &'a str,
	state: &'a str,
	nonce: &'a str,
	scope: &'a str,
	response_type: &'a str,
	access_type: &'a str,
	code_challenge_method: &'a str,
	code_challenge: &'a str,
	redirect_uri: Option<&'a str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	prompt: Option<&'a str>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GrantCookie<'a> {
	client_id: Cow<'a, str>,
	state: Cow<'a, str>,
	nonce: Cow<'a, str>,
	redirect_uri: Cow<'a, str>,
}

static GRANT_SESSION_COOKIE: &str = "tuwunel_grant_session";

/// Path attribute for the grant-session cookie. The set and removal cookies
/// must use the same value: a cookie is only replaced (and thus removed) when
/// its name, domain and path all match (RFC 6265 5.3), and a Set-Cookie
/// without an explicit path defaults to the request-URI's directory.
fn grant_session_cookie_path(callback_url: Option<&Url>) -> &str {
	callback_url.map(Url::path).unwrap_or("/")
}

/// # `GET /_matrix/client/v3/login/sso/redirect`
///
/// A web-based Matrix client should instruct the user’s browser to navigate to
/// this endpoint in order to log in via SSO.
#[tracing::instrument(
	name = "sso_login",
	level = "debug",
	skip_all,
	fields(%client),
)]
pub(crate) async fn sso_login_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<sso_login::v3::Request>,
) -> Result<sso_login::v3::Response> {
	if services.config.sso_custom_providers_page {
		return Err!(Request(NotImplemented(
			"sso_custom_providers_page has been enabled but this URL has not been overridden \
			 with any custom page listing the available providers..."
		)));
	}

	let redirect_url = body.body.redirect_url;
	let action = body.body.action;
	let default_idp_id = services
		.oauth
		.providers
		.get_default_id()
		.unwrap_or_default();

	handle_sso_login(&services, &client, default_idp_id, redirect_url, None, action)
		.map_ok(|response| sso_login::v3::Response {
			location: response.location,
			cookie: response.cookie,
		})
		.await
}

/// # `GET /_matrix/client/v3/login/sso/redirect/{idpId}`
///
/// This endpoint is the same as /login/sso/redirect, though with an IdP ID from
/// the original identity_providers array to inform the server of which IdP the
/// client/user would like to continue with.
#[tracing::instrument(
	name = "sso_login_with_provider",
	level = "info",
	skip_all,
	ret(level = "debug")
	fields(
		%client,
		idp_id = body.body.idp_id,
	),
)]
pub(crate) async fn sso_login_with_provider_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<sso_login_with_provider::v3::Request>,
) -> Result<sso_login_with_provider::v3::Response> {
	let idp_id = body.body.idp_id;
	let redirect_url = body.body.redirect_url;
	let login_token = body.body.login_token;
	let action = body.body.action;

	handle_sso_login(&services, &client, idp_id, redirect_url, login_token, action).await
}

async fn handle_sso_login(
	services: &Services,
	_client: &IpAddr,
	idp_id: String,
	redirect_url: String,
	login_token: Option<String>,
	action: Option<SsoRedirectAction>,
) -> Result<sso_login_with_provider::v3::Response> {
	let redirect_url: Url = redirect_url.parse().map_err(|e| {
		err!(Request(InvalidParam(debug_warn!(
			?e,
			?redirect_url,
			"Failed to parse redirect_url.",
		))))
	})?;

	let provider = services.oauth.providers.get(&idp_id).await?;
	let sess_id = utils::random_string(SESSION_ID_LENGTH);
	let query_nonce = utils::random_string(CODE_VERIFIER_LENGTH);
	let cookie_nonce = utils::random_string(CODE_VERIFIER_LENGTH);
	let code_verifier = utils::random_string(CODE_VERIFIER_LENGTH);
	let code_challenge = b64.encode(sha256::hash(code_verifier.as_bytes()));
	let callback_uri = provider.callback_url.as_ref().map(Url::as_str);
	let scope = provider.scope.iter().join(" ");
	let prompt = action
		.filter(|_| provider.forward_action_prompt)
		.and_then(|action| matches!(action, SsoRedirectAction::Register).then_some("create"));

	let query = GrantQuery {
		client_id: &provider.client_id,
		state: &sess_id,
		nonce: &query_nonce,
		access_type: "online",
		response_type: "code",
		code_challenge_method: "S256",
		code_challenge: &code_challenge,
		redirect_uri: callback_uri,
		prompt,
		scope: scope
			.is_empty()
			.then_some("openid email profile")
			.unwrap_or(scope.as_str()),
	};

	let location = provider
		.authorization_url
		.clone()
		.map(|mut location| {
			let query = serde_html_form::to_string(&query).ok();
			location.set_query(query.as_deref());
			if !provider.extra_authorization_parameters.is_empty() {
				// Base wins on key collision so extras cannot disable CSRF/PKCE.
				let merged: BTreeMap<String, String> = provider
					.extra_authorization_parameters
					.clone()
					.into_iter()
					.chain(
						location
							.query_pairs()
							.map(|(k, v)| (k.into_owned(), v.into_owned())),
					)
					.collect();

				location.set_query(None);
				location.query_pairs_mut().extend_pairs(&merged);
			}
			location
		})
		.ok_or_else(|| {
			err!(Config("authorization_url", "Missing required IdentityProvider config"))
		})?;

	let cookie_val = GrantCookie {
		client_id: query.client_id.into(),
		state: query.state.into(),
		nonce: cookie_nonce.as_str().into(),
		redirect_uri: redirect_url.as_str().into(),
	};

	let cookie_path = grant_session_cookie_path(provider.callback_url.as_ref());

	let cookie_max_age = provider
		.grant_session_duration
		.map(Duration::from_secs)
		.expect("Defaulted to Some value during configure_idp()")
		.try_into()
		.expect("std::time::Duration to time::Duration conversion failure");

	let cookie = Cookie::build((GRANT_SESSION_COOKIE, serde_html_form::to_string(&cookie_val)?))
		.path(cookie_path)
		.max_age(cookie_max_age)
		.same_site(SameSite::None)
		.secure(true)
		.http_only(true)
		.build()
		.to_string()
		.into();

	let session = Session {
		idp_id: Some(idp_id),
		sess_id: Some(sess_id.clone()),
		redirect_url: Some(redirect_url),
		code_verifier: Some(code_verifier),
		query_nonce: Some(query_nonce),
		cookie_nonce: Some(cookie_nonce),
		authorize_expires_at: provider
			.grant_session_duration
			.map(Duration::from_secs)
			.map(timepoint_from_now)
			.transpose()?,

		user_id: login_token
			.as_deref()
			.map_async(|token| services.users.find_from_login_token(token))
			.map(FlatOk::flat_ok)
			.await,

		..Default::default()
	};

	services.oauth.sessions.put(&session).await;

	Ok(sso_login_with_provider::v3::Response {
		location: location.into(),
		cookie: Some(cookie),
	})
}

#[tracing::instrument(
	name = "sso_callback"
	level = "debug",
	skip_all,
	fields(
		%client,
		cookie = ?body.cookie,
		body = ?body.body,
	),
)]
pub(crate) async fn sso_callback_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<sso_callback::unstable::Request>,
) -> Result<sso_callback::unstable::Response> {
	let sess_id = body
		.body
		.state
		.as_deref()
		.ok_or_else(|| err!(Request(Forbidden("Missing sess_id in callback."))))?;

	let code = body
		.body
		.code
		.as_deref()
		.ok_or_else(|| err!(Request(Forbidden("Missing code in callback."))))?;

	let session = services
		.oauth
		.sessions
		.get(sess_id)
		.map_err(|_| err!(Request(Forbidden("Invalid state in callback"))));

	let provider = services
		.oauth
		.providers
		.get(body.body.idp_id.as_str());

	let (provider, session) = try_join(provider, session).await.log_err()?;
	let idp_id = provider.id();

	if session.sess_id.as_deref() != Some(sess_id) {
		return Err!(Request(Unauthorized("Session ID {sess_id:?} not recognized.")));
	}

	if session.idp_id.as_deref() != Some(idp_id) {
		return Err!(Request(Unauthorized(
			"Identity Provider {idp_id:?} session not recognized."
		)));
	}

	if session
		.authorize_expires_at
		.is_some_and(timepoint_has_passed)
	{
		return Err!(Request(Unauthorized("Authorization grant session has expired.")));
	}

	if provider.check_cookie {
		validate_session_cookie(&body.cookie, &provider, &session, sess_id)?;
	}

	let token_response = services
		.oauth
		.request_token((&provider, &session), code)
		.await?;

	let session = apply_token_response(session, token_response)?;

	let userinfo = services
		.oauth
		.request_userinfo((&provider, &session))
		.await
		.or_else(|error| {
			if provider.brand != "appleoidc" {
				return Err(error);
			}

			debug_warn!(
				?error,
				idp_id = provider.id(),
				"Failed to fetch Apple userinfo endpoint; falling back to id_token claims.",
			);

			native_apple::decode_userinfo_from_id_token(&session).map_err(|decode_error| {
				debug_warn!(
					?decode_error,
					idp_id = provider.id(),
					"Failed to decode Apple id_token fallback.",
				);
				error
			})
		})?;

	let (user_id, session) =
		complete_sso_session(&services, &provider, session, userinfo).await?;

	let cookie = Cookie::build((GRANT_SESSION_COOKIE, EMPTY))
		.path(grant_session_cookie_path(provider.callback_url.as_ref()))
		.removal()
		.build()
		.to_string()
		.into();

	if let Some(redirect_url) = session
		.redirect_url
		.as_ref()
		.filter(|url| url.scheme() == "uiaa")
	{
		return handle_uiaa(&services, &user_id, cookie, redirect_url).await;
	}

	let next_idp_url = chain_next_idp_url(&services, &provider, &session, idp_id);

	let location = finalize_login_redirect(&services, &session, next_idp_url, &user_id)?;

	Ok(sso_callback::unstable::Response { location, cookie: Some(cookie) })
}

fn validate_session_cookie(
	cookies: &CookieJar,
	provider: &Provider,
	session: &Session,
	sess_id: &str,
) -> Result {
	let client_id = &provider.client_id;
	let cookie = cookies
		.get(GRANT_SESSION_COOKIE)
		.map(Cookie::value)
		.map(serde_html_form::from_str::<GrantCookie<'_>>)
		.transpose()?
		.ok_or_else(|| err!(Request(Unauthorized("Missing cookie {GRANT_SESSION_COOKIE:?}"))))?;

	if cookie.client_id.as_ref() != client_id.as_str() {
		return Err!(Request(Unauthorized("Client ID {client_id:?} cookie mismatch.")));
	}

	if Some(cookie.nonce.as_ref()) != session.cookie_nonce.as_deref() {
		return Err!(Request(Unauthorized("Cookie nonce does not match session state.")));
	}

	if cookie.state.as_ref() != sess_id {
		return Err!(Request(Unauthorized("Session ID {sess_id:?} cookie mismatch.")));
	}

	Ok(())
}

fn apply_token_response(session: Session, token: TokenResponse) -> Result<Session> {
	let expires_at = token
		.expires_in
		.map(Duration::from_secs)
		.map(timepoint_from_now)
		.transpose()?;

	let refresh_token_expires_at = token
		.refresh_token_expires_in
		.map(Duration::from_secs)
		.map(timepoint_from_now)
		.transpose()?;

	Ok(Session {
		scope: token.scope,
		token_type: token.token_type,
		access_token: token.access_token,
		id_token: token.id_token,
		expires_at,
		refresh_token: token.refresh_token,
		refresh_token_expires_at,
		..session
	})
}

/// Locate any prior session bound to the same upstream identity, to preserve
/// one session and its `user_id` association per identity.
async fn existing_identity_session(
	services: &Services,
	unique_id: &str,
) -> Result<(Option<OwnedUserId>, Option<String>)> {
	match services
		.oauth
		.sessions
		.get_by_unique_id(unique_id)
		.await
	{
		| Ok(session) => Ok((session.user_id, session.sess_id)),
		| Err(error) if !error.is_not_found() => Err(error),
		| Err(_) => Ok((None, None)),
	}
}

fn chain_next_idp_url(
	services: &Services,
	provider: &Provider,
	session: &Session,
	idp_id: &str,
) -> Option<Url> {
	services
		.config
		.identity_provider
		.values()
		.filter(|idp| idp.default || services.config.single_sso)
		.skip_while(|idp| idp.id() != idp_id)
		.nth(1)
		.map(IdentityProvider::id)
		.and_then(|next_idp| {
			provider.callback_url.clone().map(|mut url| {
				let path = format!("/_matrix/client/v3/login/sso/redirect/{next_idp}");
				url.set_path(&path);

				if let Some(redirect_url) = session.redirect_url.as_ref() {
					url.query_pairs_mut()
						.append_pair("redirectUrl", redirect_url.as_str());
				}

				url
			})
		})
}

fn finalize_login_redirect(
	services: &Services,
	session: &Session,
	next_idp_url: Option<Url>,
	user_id: &UserId,
) -> Result<String> {
	let login_token = utils::random_string(TOKEN_LENGTH);
	let _login_token_expires_in = services
		.users
		.create_login_token(user_id, &login_token);

	let location = next_idp_url
		.or_else(|| session.redirect_url.clone())
		.ok_or_else(|| err!(Request(InvalidParam("Missing redirect URL in session data"))))?
		.query_pairs_mut()
		.append_pair("loginToken", &login_token)
		.finish()
		.to_string();

	Ok(location)
}

/// Map an authenticated SSO/OIDC identity onto a local account and return the
/// resulting user together with the committed session. Shared by the browser
/// SSO callback and the native Apple login endpoint.
async fn complete_sso_session(
	services: &Services,
	provider: &Provider,
	mut session: Session,
	userinfo: UserInfo,
) -> Result<(OwnedUserId, Session)> {
	let sess_id = session
		.sess_id
		.clone()
		.ok_or_else(|| err!(Request(InvalidParam("Missing SSO session id."))))?;

	let unique_id = unique_id_sub((provider, &userinfo.sub))?;

	let (old_user_id, old_sess_id) = existing_identity_session(services, &unique_id).await?;

	session.user_info = Some(userinfo.clone());

	let user_id = match (session.user_id.take(), old_user_id) {
		| (Some(user_id), ..) | (None, Some(user_id)) => user_id,
		| (None, None) => decide_user_id(services, provider, &userinfo, &unique_id).await?,
	};

	session.user_id = Some(user_id.clone());

	if !services.users.exists(&user_id).await {
		let origin = match provider.registration {
			| true => "sso",
			// Present in LDAP is an existing user to provision, not a new registration.
			| false if ldap_user_exists(services, &user_id).await => "ldap",
			| false =>
				return Err!(Request(Forbidden("Registration from this provider is disabled"))),
		};

		register_user(services, provider, &session, &userinfo, &user_id, origin).await?;
	}

	services.oauth.sessions.put(&session).await;
	if services
		.users
		.maybe_repair_legacy_sso_origin(&user_id)
		.await
	{
		info!("Repaired legacy SSO-origin metadata for {user_id}");
	}

	if let Some(old_sess_id) = old_sess_id
		.as_deref()
		.filter(is_not_equal_to!(&sess_id))
	{
		services.oauth.sessions.delete(old_sess_id).await;
	}

	if services
		.users
		.maybe_reactivate_deactivated_sso(&user_id)
		.await?
	{
		info!("Reactivated deactivated SSO account {user_id}");
	}

	if !services.users.is_active_local(&user_id).await {
		return Err!(Request(UserDeactivated("This user has been deactivated.")));
	}

	Ok((user_id, session))
}

async fn handle_uiaa(
	services: &Services,
	user_id: &UserId,
	cookie: Cow<'static, str>,
	redirect_url: &Url,
) -> Result<sso_callback::unstable::Response> {
	let uiaa_session_id = redirect_url.path();

	// Find the UIAA session by its ID. SECURITY: Ensure the user authenticating via
	// SSO is the owner of the UIAA session
	let (user_id, device_id, mut uiaainfo) = services
		.uiaa
		.get_uiaa_session_by_session_id(uiaa_session_id)
		.await
		.filter(|(db_user_id, ..)| user_id.eq(db_user_id))
		.ok_or_else(|| err!(Request(Forbidden("UIAA session not found."))))?;

	// MSC4312 m.oauth flow → mark OAuth.
	let has_oauth_flow = uiaainfo
		.flows
		.iter()
		.any(|f| f.stages.contains(&AuthType::OAuth));

	// Mark the completed step based on the UIAA session's flow.
	if has_oauth_flow && !uiaainfo.completed.contains(&AuthType::OAuth) {
		// Grant 10-minute bypass for cross-signing key replacement (like Synapse).
		services
			.users
			.allow_cross_signing_replacement(&user_id);

		uiaainfo.completed.push(AuthType::OAuth);
	}

	// Legacy m.login.sso flow → mark Sso.
	let has_sso_flow = uiaainfo
		.flows
		.iter()
		.any(|f| f.stages.contains(&AuthType::Sso));

	if has_sso_flow && !uiaainfo.completed.contains(&AuthType::Sso) {
		uiaainfo.completed.push(AuthType::Sso);
	}

	services
		.uiaa
		.update_uiaa_session(&user_id, &device_id, uiaa_session_id, Some(&uiaainfo));

	// Redirect back to the fallback page to render the success HTML
	let location =
		format!("/_matrix/client/v3/auth/m.login.sso/fallback/web?session={uiaa_session_id}");

	Ok(sso_callback::unstable::Response { location, cookie: Some(cookie) })
}

async fn ldap_user_exists(services: &Services, user_id: &UserId) -> bool {
	cfg!(feature = "ldap")
		&& services.config.ldap.enable
		&& services
			.users
			.search_ldap(user_id)
			.await
			.log_err()
			.is_ok_and(|dns| !dns.is_empty())
}

#[tracing::instrument(
	name = "register",
	level = INFO_SPAN_LEVEL,
	skip_all,
	fields(user_id, userinfo)
)]
async fn register_user(
	services: &Services,
	provider: &Provider,
	session: &Session,
	userinfo: &UserInfo,
	user_id: &UserId,
	origin: &str,
) -> Result {
	debug_info!(%user_id, "Creating new user account...");

	services
		.users
		.full_register(Register {
			user_id: Some(user_id),
			password: Some(PASSWORD_SENTINEL),
			origin: Some(origin),
			displayname: userinfo.name.as_deref(),
			grant_first_user_admin: true,
			..Default::default()
		})
		.await?;

	if let Some(avatar_url) = userinfo
		.avatar_url
		.as_deref()
		.or(userinfo.picture.as_deref())
	{
		set_avatar(services, provider, session, userinfo, user_id, avatar_url)
			.await
			.ok();
	}

	let idp_id = provider.id();
	let idp_name = provider
		.name
		.as_deref()
		.unwrap_or(provider.brand.as_str());

	// log in conduit admin channel if a non-guest user registered
	let notice =
		format!("New user \"{user_id}\" registered on this server via {idp_name} ({idp_id})");

	info!("{notice}");
	services.admin.notify(&notice).await;

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all, fields(user_id, avatar_url))]
async fn set_avatar(
	services: &Services,
	_provider: &Provider,
	_session: &Session,
	_userinfo: &UserInfo,
	user_id: &UserId,
	avatar_url: &str,
) -> Result {
	use reqwest::Response;

	let response = services
		.client
		.default
		.get(avatar_url)
		.send()
		.await
		.and_then(Response::error_for_status)?;

	let content_type = response
		.headers()
		.get(CONTENT_TYPE)
		.map(HeaderValue::to_str)
		.flat_ok()
		.map(ToOwned::to_owned);

	let mxc = Mxc {
		server_name: services.globals.server_name(),
		media_id: &utils::random_string(MXC_LENGTH),
	};

	let content_disposition = make_content_disposition(None, content_type.as_deref(), None);
	let limit = services.server.config.max_response_size;
	let bytes = read_response_capped(response, limit).await?;
	services
		.media
		.create(&mxc, Some(user_id), Some(&content_disposition), content_type.as_deref(), &bytes)
		.await?;

	let mxc_uri: OwnedMxcUri = mxc.to_string().into();
	services
		.profile
		.set_avatar_url(user_id, Some(&mxc_uri), None)
		.await?;

	Ok(())
}

#[tracing::instrument(
	level = "debug",
	ret(level = "debug")
	skip_all,
	fields(user),
)]
async fn decide_user_id(
	services: &Services,
	provider: &Provider,
	userinfo: &UserInfo,
	unique_id: &str,
) -> Result<OwnedUserId> {
	if let Some(user_id) = services
		.oauth
		.sessions
		.find_user_association_pending(provider.id(), userinfo)
	{
		debug_info!(
			provider = ?provider.id(),
			?user_id,
			?userinfo,
			"Matched pending association"
		);

		return Ok(user_id);
	}

	let explicit = |claim: &str| provider.userid_claims.contains(claim);

	let allowed = |claim: &str| provider.userid_claims.is_empty() || explicit(claim);

	let choices = [
		explicit("sub")
			.then_some(userinfo.sub.as_str())
			.map(str::to_lowercase),
		userinfo
			.preferred_username
			.as_deref()
			.map(str::to_lowercase)
			.filter(|_| allowed("preferred_username")),
		userinfo
			.username
			.as_deref()
			.map(str::to_lowercase)
			.filter(|_| allowed("username")),
		userinfo
			.nickname
			.as_deref()
			.map(str::to_lowercase)
			.filter(|_| allowed("nickname")),
		provider
			.brand
			.eq(&"github")
			.then_some(userinfo.sub.as_str())
			.map(str::to_lowercase)
			.filter(|_| allowed("login")),
		userinfo
			.email
			.as_deref()
			.and_then(|email| email.split_once('@'))
			.map(at!(0))
			.map(str::to_lowercase)
			.filter(|_| allowed("email")),
	];

	for choice in choices.into_iter().flatten() {
		if let Some(user_id) = try_user_id(services, provider, &choice, false).await {
			return Ok(user_id);
		}
	}

	let length = Some(15..23);
	let unique_id = truncate_deterministic(unique_id, length).to_lowercase();
	if let Some(user_id) = try_user_id(services, provider, &unique_id, true).await {
		return Ok(user_id);
	}

	Err!(Request(UserInUse("User ID is not available.")))
}

#[tracing::instrument(level = "debug", skip_all, fields(username))]
async fn try_user_id(
	services: &Services,
	provider: &Provider,
	username: &str,
	unique_id: bool,
) -> Option<OwnedUserId> {
	let server_name = services.globals.server_name();
	let user_id = parse_user_id(server_name, username)
		.inspect_err(|e| warn!(?username, "Username invalid: {e}"))
		.ok()?;

	if services
		.config
		.forbidden_usernames
		.is_match(username)
	{
		warn!(?username, "Username forbidden.");
		return None;
	}

	if services.users.exists(&user_id).await {
		if provider.trusted {
			info!(
				?username,
				provider = ?provider.brand,
				"Authorizing trusted provider access to existing account."
			);

			return Some(user_id);
		}

		if services
			.users
			.origin(&user_id)
			.await
			.ok()
			.is_none_or(|origin| origin != "sso")
		{
			debug_warn!(?username, "Existing username has non-sso origin.");
			return None;
		}

		if !unique_id {
			debug_warn!(?username, "Username exists.");
			return None;
		}
	} else if unique_id && !provider.unique_id_fallbacks {
		debug_warn!(
			?username,
			provider = ?provider.brand,
			"Unique ID fallbacks disabled.",
		);

		return None;
	}

	Some(user_id)
}

fn parse_user_id(server_name: &ServerName, username: &str) -> Result<OwnedUserId> {
	match UserId::parse_with_server_name(username, server_name) {
		| Err(e) => {
			Err!(Request(InvalidUsername(debug_error!("Username {username} is not valid: {e}"))))
		},
		| Ok(user_id) => match user_id.validate_strict() {
			| Ok(()) => Ok(user_id),
			| Err(e) => Err!(Request(InvalidUsername(debug_error!(
				"Username {username} contains disallowed characters or spaces: {e}"
			)))),
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn grant_session_cookie_path_uses_the_callback_path() {
		let callback = Url::parse(
			"https://matrix.example/_matrix/client/unstable/login/sso/callback/tuwunel_abc",
		)
		.expect("valid URL");

		assert_eq!(
			grant_session_cookie_path(Some(&callback)),
			"/_matrix/client/unstable/login/sso/callback/tuwunel_abc"
		);
		assert_eq!(grant_session_cookie_path(None), "/");
	}

	#[test]
	fn grant_session_removal_cookie_path_matches_the_set_cookie_path() {
		let callback = Url::parse(
			"https://matrix.example/_matrix/client/unstable/login/sso/callback/tuwunel_abc",
		)
		.expect("valid URL");

		let set = Cookie::build((GRANT_SESSION_COOKIE, "grant"))
			.path(grant_session_cookie_path(Some(&callback)))
			.build();

		let removal = Cookie::build((GRANT_SESSION_COOKIE, EMPTY))
			.path(grant_session_cookie_path(Some(&callback)))
			.removal()
			.build();

		assert_eq!(set.path(), removal.path());
		assert!(
			removal
				.to_string()
				.contains("Path=/_matrix/client/unstable/login/sso/callback/tuwunel_abc"),
			"unexpected removal cookie: {removal}"
		);
	}
}
