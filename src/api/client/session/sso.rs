use std::{borrow::Cow, net::IpAddr, time::Duration};

use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use axum_extra::extract::cookie::{Cookie, SameSite};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use futures::{FutureExt, StreamExt, TryFutureExt, future::try_join};
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use ruma::{
	Mxc, OwnedMxcUri, OwnedRoomId, OwnedUserId, ServerName, UserId,
	api::client::session::{sso_callback, sso_login, sso_login_with_provider},
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Result, at,
	config::IdentityProvider,
	debug::INFO_SPAN_LEVEL,
	debug_info, debug_warn, err, info,
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
	media::MXC_LENGTH,
	oauth::{
		CODE_VERIFIER_LENGTH, Provider, SESSION_ID_LENGTH, Session, UserInfo, unique_id_sub,
	},
	users::Register,
};
use url::Url;

use super::TOKEN_LENGTH;
use crate::Ruma;

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
}

#[derive(Debug, Deserialize, Serialize)]
struct GrantCookie<'a> {
	client_id: Cow<'a, str>,
	state: Cow<'a, str>,
	nonce: Cow<'a, str>,
	redirect_uri: Cow<'a, str>,
}

static GRANT_SESSION_COOKIE: &str = "tuwunel_grant_session";

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
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<sso_login::v3::Request>,
) -> Result<sso_login::v3::Response> {
	if services.config.sso_custom_providers_page {
		return Err!(Request(NotImplemented(
			"sso_custom_providers_page has been enabled but this URL has not been overridden \
			 with any custom page listing the available providers..."
		)));
	}

	let default_idp_id = services
		.config
		.identity_provider
		.values()
		.find(|idp| idp.default)
		.or_else(|| services.config.identity_provider.values().next())
		.map(IdentityProvider::id)
		.map(ToOwned::to_owned)
		.unwrap_or_default();

	let redirect_url = body.body.redirect_url;

	handle_sso_login(&services, &client, default_idp_id, redirect_url, None)
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
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<sso_login_with_provider::v3::Request>,
) -> Result<sso_login_with_provider::v3::Response> {
	let idp_id = body.body.idp_id;
	let redirect_url = body.body.redirect_url;
	let login_token = body.body.login_token;

	handle_sso_login(&services, &client, idp_id, redirect_url, login_token).await
}

async fn handle_sso_login(
	services: &Services,
	_client: &IpAddr,
	idp_id: String,
	redirect_url: String,
	login_token: Option<String>,
) -> Result<sso_login_with_provider::v3::Response> {
	let Ok(redirect_url) = redirect_url.parse::<Url>() else {
		return Err!(Request(InvalidParam("Invalid redirect_url")));
	};

	let provider = services.oauth.providers.get(&idp_id).await?;
	let sess_id = utils::random_string(SESSION_ID_LENGTH);
	let query_nonce = utils::random_string(CODE_VERIFIER_LENGTH);
	let cookie_nonce = utils::random_string(CODE_VERIFIER_LENGTH);
	let code_verifier = utils::random_string(CODE_VERIFIER_LENGTH);
	let code_challenge = b64.encode(sha256::hash(code_verifier.as_bytes()));
	let callback_uri = provider.callback_url.as_ref().map(Url::as_str);
	let scope = provider.scope.iter().join(" ");

	let query = GrantQuery {
		client_id: &provider.client_id,
		state: &sess_id,
		nonce: &query_nonce,
		access_type: "online",
		response_type: "code",
		code_challenge_method: "S256",
		code_challenge: &code_challenge,
		redirect_uri: callback_uri,
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

	let cookie_path = provider
		.callback_url
		.as_ref()
		.map(Url::path)
		.unwrap_or("/");

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
	InsecureClientIp(client): InsecureClientIp,
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
	let client_id = &provider.client_id;
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
		.map(timepoint_has_passed)
		.unwrap_or(false)
	{
		return Err!(Request(Unauthorized("Authorization grant session has expired.")));
	}

	let cookie = body
		.cookie
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

	// Request access token.
	let token_response = services
		.oauth
		.request_token((&provider, &session), code)
		.await?;

	let token_expires_at = token_response
		.expires_in
		.map(Duration::from_secs)
		.map(timepoint_from_now)
		.transpose()?;

	let refresh_token_expires_at = token_response
		.refresh_token_expires_in
		.map(Duration::from_secs)
		.map(timepoint_from_now)
		.transpose()?;

	// Update the session with access token results
	let session = Session {
		scope: token_response.scope,
		token_type: token_response.token_type,
		access_token: token_response.access_token,
		expires_at: token_expires_at,
		refresh_token: token_response.refresh_token,
		refresh_token_expires_at,
		..session
	};

	// Request userinfo claims.
	let userinfo = services
		.oauth
		.request_userinfo((&provider, &session))
		.await?;

	let unique_id = unique_id_sub((&provider, &userinfo.sub))?;

	// Check for an existing session from this identity. We want to maintain one
	// session for each identity and keep the newer one which has up-to-date state
	// and access.
	let (old_user_id, old_sess_id) = match services
		.oauth
		.sessions
		.get_by_unique_id(&unique_id)
		.await
	{
		| Ok(session) => (session.user_id, session.sess_id),
		| Err(error) if !error.is_not_found() => return Err(error),
		| Err(_) => (None, None),
	};

	// Update the session with userinfo
	let session = Session {
		user_info: Some(userinfo.clone()),
		..session
	};

	// Keep the user_id from the old session as best as possible.
	let user_id = match (session.user_id, old_user_id) {
		| (Some(user_id), ..) | (None, Some(user_id)) => user_id,
		| (None, None) => decide_user_id(&services, &provider, &userinfo, &unique_id).await?,
	};

	// Update the session with user_id
	let session = Session {
		user_id: Some(user_id.clone()),
		..session
	};

	// Attempt to register a non-existing user.
	if !services.users.exists(&user_id).await {
		register_user(&services, &provider, &session, &userinfo, &user_id).await?;
	}

	// Commit the updated session.
	services.oauth.sessions.put(&session).await;

	// Delete any old session.
	if let Some(old_sess_id) = old_sess_id
		&& sess_id != old_sess_id
	{
		services.oauth.sessions.delete(&old_sess_id).await;
	}

	if !services.users.is_active_local(&user_id).await {
		return Err!(Request(UserDeactivated("This user has been deactivated.")));
	}

	let cookie = Cookie::build((GRANT_SESSION_COOKIE, EMPTY))
		.removal()
		.build()
		.to_string()
		.into();

	// Determine the next provider to chain after this one.
	let next_idp_url = services
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
		});

	// Allow the user to login to Matrix.
	let login_token = utils::random_string(TOKEN_LENGTH);
	let _login_token_expires_in = services
		.users
		.create_login_token(&user_id, &login_token);

	let location = next_idp_url
		.or(session.redirect_url)
		.as_ref()
		.ok_or_else(|| err!(Request(InvalidParam("Missing redirect URL in session data"))))?
		.clone()
		.query_pairs_mut()
		.append_pair("loginToken", &login_token)
		.finish()
		.to_string();

	Ok(sso_callback::unstable::Response { location, cookie: Some(cookie) })
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
) -> Result {
	debug_info!(%user_id, "Creating new user account...");

	services
		.users
		.full_register(Register {
			user_id: Some(user_id),
			password: Some("*"),
			origin: Some("sso"),
			displayname: userinfo.name.as_deref(),
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
		format!("New user \"{user_id}\" registered on this server via {idp_name} ({idp_id})",);

	info!("{notice}");
	if services.server.config.admin_room_notices {
		services.admin.notice(&notice).await;
	}

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
	let bytes = response.bytes().await?;
	services
		.media
		.create(&mxc, Some(user_id), Some(&content_disposition), content_type.as_deref(), &bytes)
		.await?;

	let all_joined_rooms: Vec<OwnedRoomId> = services
		.state_cache
		.rooms_joined(user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let mxc_uri: OwnedMxcUri = mxc.to_string().into();
	services
		.users
		.update_avatar_url(user_id, Some(&mxc_uri), None, &all_joined_rooms)
		.await;

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

	let allowed =
		|claim: &str| provider.userid_claims.is_empty() || provider.userid_claims.contains(claim);

	let choices = [
		userinfo
			.preferred_username
			.as_deref()
			.map(str::to_lowercase)
			.filter(|_| allowed("preferred_username")),
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
		if let Some(user_id) = try_user_id(services, &choice, false).await {
			return Ok(user_id);
		}
	}

	let length = Some(15..23);
	let unique_id = truncate_deterministic(unique_id, length).to_lowercase();
	if let Some(user_id) = try_user_id(services, &unique_id, true).await {
		return Ok(user_id);
	}

	Err!(Request(UserInUse("User ID is not available.")))
}

#[tracing::instrument(level = "debug", skip_all, fields(username))]
async fn try_user_id(
	services: &Services,
	username: &str,
	may_exist: bool,
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
		debug_warn!(?username, "Username exists.");

		if services
			.users
			.origin(&user_id)
			.await
			.ok()
			.is_none_or(|origin| origin != "sso")
		{
			debug_warn!(?username, "Username has non-sso origin.");
			return None;
		}

		if !may_exist {
			return None;
		}
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
