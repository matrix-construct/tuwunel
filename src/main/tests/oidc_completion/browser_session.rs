use std::{iter::once, time::Duration};

use futures::{StreamExt, TryStreamExt};
use reqwest::{
	Client, Response, StatusCode, Url,
	header::{
		CACHE_CONTROL, COOKIE, HeaderName, HeaderValue, LOCATION, REFERRER_POLICY, SET_COOKIE,
	},
	redirect::Policy,
};
use serde_json::{Value, from_str};
use tokio::time::sleep;
use tuwunel_core::{
	Config, Result,
	ruma::{OwnedDeviceId, OwnedUserId, UserId},
};
use tuwunel_database::Interfix;
use tuwunel_service::{Services, users::Register};

use super::{PASSWORD, USERNAME, location, parameter, post_form, register_client};

type Row = (Vec<u8>, Vec<u8>);

/// What every step shares.
///
/// The server, a browser without a cookie store, and the client the device
/// grants belong to.
struct Harness<'a> {
	services: &'a Services,
	client: Client,
	base: &'a str,
	client_id: String,
}

/// What the switch steps leave for the steps after them.
///
/// A fresh cookie for the main account, the other account's cookie, and the
/// other account's token response.
struct Switched {
	cookie: String,
	other_cookie: String,
	other_tokens: String,
}

const COOKIE_NAME: &str = "tuwunel_oidc_session";

const SESSIONS: &str = "useridbrowsersession_createdat";

const OTHER: &str = "browsersessionother";

const GONE: &str = "browsersessiongone";

/// A device approval from a browser signed in by password skips the password.
///
/// The session lives only in that browser's cookie and stands in for the
/// password page alone: consent still takes its click, the code is checked only
/// once authenticated, and a password change, deactivation or sign-out
/// everywhere ends it. A cookie that does not name a live session changes
/// nothing about the response.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn browser_session(services: &Services, base: &str, user: &UserId) -> Result {
	let client = Client::builder()
		.redirect(Policy::none())
		.timeout(Duration::from_secs(10))
		.default_headers(
			once((HeaderName::from_static("x-real-ip"), HeaderValue::from_static("127.0.0.65")))
				.collect(),
		)
		.build()?;

	let client_id = register_client(services, "https://trusted.example/browser-session").await?;
	let harness = Harness { services, client, base, client_id };
	let other = register(services, OTHER).await?;
	let cookie = sign_in_sets_cookie(&harness, user).await?;

	approve_with_session(&harness, user, &cookie).await?;
	cookie_opens_no_other_page(&harness, &cookie).await?;
	dead_cookies_are_ignored(&harness, &cookie).await?;
	wrong_code_keeps_session(&harness, &cookie).await?;

	let (other_cookie, other_tokens) = switch_account(&harness, &other, &cookie).await?;
	let cookie = switch_releases_only_its_binding(&harness, user, &other).await?;
	let switched = Switched { cookie, other_cookie, other_tokens };

	settings_turn_sessions_off(&harness, user, &switched.cookie).await?;
	revocation_ends_sessions(&harness, user, &switched).await?;
	sign_out_everywhere_ends_sessions(&harness, &other, &switched.other_tokens).await?;
	deactivation_ends_sessions(&harness).await
}

/// A password sign-in sets the session cookie and stores only a digest.
///
/// The cookie is host-only, `HttpOnly`, `Secure` and `SameSite=Lax`, scoped to
/// the OIDC path and kept for the configured lifetime; no stored key carries
/// the secret it holds.
#[tracing::instrument(level = "debug", skip_all)]
async fn sign_in_sets_cookie(
	harness @ Harness { services, .. }: &Harness<'_>,
	user: &UserId,
) -> Result<String> {
	let before = sessions(harness, user).await;
	let code = user_code(harness)?;
	let response = sign_in(harness, &code, USERNAME).await?;

	assert_eq!(response.status(), StatusCode::SEE_OTHER);
	assert_eq!(redirect(harness, &response)?.path(), "/_tuwunel/oidc/device_callback");
	assert_private(&response);

	let header = set_cookie(&response).expect("sign-in sets the session cookie");
	let attributes: Vec<_> = header.split("; ").skip(1).collect();

	assert!(header.starts_with(&format!("{COOKIE_NAME}=")));
	for attribute in
		["HttpOnly", "Secure", "SameSite=Lax", "Path=/_tuwunel/oidc", "Max-Age=2592000"]
	{
		assert!(attributes.contains(&attribute), "{attribute} missing from {header}");
	}

	assert!(!header.contains("Domain"));

	let cookie = pair(&header);
	let secret = cookie
		.trim_start_matches(&format!("{COOKIE_NAME}="))
		.split('.')
		.next()
		.expect("session secret");

	let keys: Vec<Vec<u8>> = services.db[SESSIONS]
		.keys_prefix_raw(&(user, Interfix))
		.map_ok(<[u8]>::to_vec)
		.try_collect()
		.await?;

	assert_eq!(secret.len(), 32);
	assert_eq!(keys.len(), before.saturating_add(1));
	assert!(
		keys.iter()
			.flat_map(|key| key.windows(secret.len()))
			.all(|bytes| bytes != secret.as_bytes())
	);

	Ok(cookie)
}

/// A second grant reaches consent through the session alone.
///
/// The verify answer carries a login token in place of the password page, and
/// neither GET touches the session; the consent page still asks, nothing is
/// issued before its click, and once approved the grant issues working tokens
/// for a device no provider is tagged on.
#[tracing::instrument(level = "debug", skip_all)]
async fn approve_with_session(
	harness @ Harness { services, client_id, .. }: &Harness<'_>,
	user: &UserId,
	cookie: &str,
) -> Result {
	let grant = services
		.oauth
		.get_server()?
		.create_device_grant(client_id, "openid");

	let stored = rows(harness, user).await?;
	let response = verify(harness, &grant.user_code, Some(cookie)).await?;
	let callback = redirect(harness, &response)?;

	assert_eq!(response.status(), StatusCode::SEE_OTHER);
	assert!(set_cookie(&response).is_none());
	assert_private(&response);
	assert_eq!(callback.path(), "/_tuwunel/oidc/device_callback");
	assert_eq!(parameter(&callback, "user_code"), grant.user_code);

	let consent = consent(harness, &callback).await?;

	assert_eq!(rows(harness, user).await?, stored);

	let pending = token_request(harness, &grant.device_code).await?;

	assert_eq!(pending.status(), StatusCode::BAD_REQUEST);
	assert_eq!(pending.json::<Value>().await?["error"], "authorization_pending");

	assert!(consent.contains("Another device is asking to sign in to your account"));
	assert!(consent.contains("Approve only if you started this sign-in yourself"));
	assert!(consent.contains(r#"value="switch""#));
	assert!(consent.contains(user.as_str()));

	let login_token = parameter(&callback, "loginToken");
	let response = decide(harness, &grant.user_code, &login_token, "approve").await?;

	assert_eq!(response.status(), StatusCode::OK);
	assert!(response.text().await?.contains("Device approved"));

	let tokens = poll(harness, &grant.device_code).await?;
	let (owner, device_id) = whoami(harness, &tokens).await?;
	let provider = services
		.users
		.get_oidc_device_idp(user, &device_id)
		.await;

	assert_eq!(owner, user.as_str());
	assert_eq!(provider, None);

	Ok(())
}

/// The session opens no page but device approval.
///
/// A client's authorization request and the account pages still ask for the
/// password, and neither answer sets or clears the cookie.
#[tracing::instrument(level = "debug", skip_all)]
async fn cookie_opens_no_other_page(
	harness @ Harness { client, base, client_id, .. }: &Harness<'_>,
	cookie: &str,
) -> Result {
	let authorize = client
		.get(format!("{base}/_tuwunel/oidc/authorize"))
		.header(COOKIE, cookie)
		.query(&[
			("client_id", client_id.as_str()),
			("redirect_uri", "https://trusted.example/browser-session"),
			("response_type", "code"),
			("scope", "openid"),
		])
		.send()
		.await?;

	let account = client
		.get(format!("{base}/_tuwunel/oidc/account"))
		.header(COOKIE, cookie)
		.send()
		.await?;

	for response in [authorize, account] {
		assert!(response.status().is_redirection());
		assert_eq!(redirect(harness, &response)?.path(), "/_tuwunel/oidc/native");
		assert!(set_cookie(&response).is_none());
	}

	Ok(())
}

/// A cookie that names no live session changes nothing.
///
/// Foreign-user, garbled and expired cookies get exactly the response a
/// request without one gets, and the session itself outlives the checks.
#[tracing::instrument(level = "debug", skip_all)]
async fn dead_cookies_are_ignored(
	harness @ Harness { services, .. }: &Harness<'_>,
	cookie: &str,
) -> Result {
	let code = user_code(harness)?;
	let foreign = cookie.replacen(USERNAME, "nobody", 1);
	let garbled = format!("{COOKIE_NAME}=garbled");

	for cookie in [foreign.as_str(), garbled.as_str()] {
		assert_ignored(harness, &code, cookie).await?;
	}

	let config = &services.config;
	let original = Config::clone(&services.server.config);

	config.replace(with_ttl(original.clone(), 1))?;
	sleep(Duration::from_millis(1100)).await;
	assert_ignored(harness, &code, cookie).await?;
	config.replace(original)?;

	assert_honored(harness, &code, cookie).await
}

/// A wrong code through a session fails as it does after a password.
///
/// The consent step shows its error page and burns the login token, and the
/// session stays live: no link can sign the browser out.
#[tracing::instrument(level = "debug", skip_all)]
async fn wrong_code_keeps_session(
	harness @ Harness { services, client, .. }: &Harness<'_>,
	cookie: &str,
) -> Result {
	let response = verify(harness, "BCDFGHJKLM", Some(cookie)).await?;
	let callback = redirect(harness, &response)?;

	assert_eq!(response.status(), StatusCode::SEE_OTHER);

	let response = client.get(callback.as_str()).send().await?;

	assert_eq!(response.status(), StatusCode::NOT_FOUND);

	services
		.users
		.find_from_login_token(&parameter(&callback, "loginToken"))
		.await
		.expect_err("a failed verify burns the login token");

	assert_honored(harness, &user_code(harness)?, cookie).await
}

/// "Use another account" hands a viewed grant to the next account.
///
/// It releases the binding the consent view made, keeping the attempt count,
/// and signs the browser out; the other account then approves the same pending
/// grant, which no switch can release once resolved. Returns the other
/// account's cookie and token response.
#[tracing::instrument(level = "debug", skip_all)]
async fn switch_account(
	harness @ Harness { services, client, base, client_id }: &Harness<'_>,
	other: &UserId,
	cookie: &str,
) -> Result<(String, String)> {
	let server = services.oauth.get_server()?;
	let grant = server.create_device_grant(client_id, "openid");
	let response = verify(harness, &grant.user_code, Some(cookie)).await?;
	let callback = redirect(harness, &response)?;

	consent(harness, &callback).await?;

	let login_token = parameter(&callback, "loginToken");
	let response = client
		.post(format!("{base}/_tuwunel/oidc/device_callback"))
		.header(COOKIE, cookie)
		.form(&[
			("user_code", grant.user_code.as_str()),
			("loginToken", login_token.as_str()),
			("action", "switch"),
		])
		.send()
		.await?;

	let removal = set_cookie(&response).expect("switch clears the cookie");
	let native = redirect(harness, &response)?;

	assert_eq!(response.status(), StatusCode::SEE_OTHER);
	assert_private(&response);
	assert_eq!(native.path(), "/_tuwunel/oidc/native");
	assert_eq!(parameter(&native, "user_code"), grant.user_code);
	assert!(removal.starts_with(&format!("{COOKIE_NAME}=;")));
	assert!(removal.contains("Max-Age=0"));
	assert!(removal.contains("Path=/_tuwunel/oidc"));
	assert_ignored(harness, &user_code(harness)?, cookie).await?;

	let released = server
		.verify_device_grant(&grant.user_code, other, None)
		.await?;

	assert_eq!(released.attempts, 2);

	let response = sign_in(harness, &grant.user_code, OTHER).await?;
	let other_cookie = pair(&set_cookie(&response).expect("other account cookie"));
	let callback = redirect(harness, &response)?;
	let consent_page = consent(harness, &callback).await?;

	assert!(consent_page.contains(other.as_str()));

	let login_token = parameter(&callback, "loginToken");
	let response = decide(harness, &grant.user_code, &login_token, "approve").await?;

	assert!(response.text().await?.contains("Device approved"));

	let resolved = "browser-session-resolved-switch";
	let _expires_in = services.users.create_login_token(other, resolved);
	let response = decide(harness, &grant.user_code, resolved, "switch").await?;

	assert_eq!(response.status(), StatusCode::FORBIDDEN);
	assert!(
		response
			.text()
			.await?
			.contains("already resolved")
	);

	let other_tokens = poll(harness, &grant.device_code).await?;

	Ok((other_cookie, other_tokens))
}

/// A switch releases only a binding made for its own user and provider.
///
/// The same account signed in through a provider, and another account, both
/// leave the grant bound, so the other account still cannot view it. Returns a
/// fresh cookie for the main account.
#[tracing::instrument(level = "debug", skip_all)]
async fn switch_releases_only_its_binding(
	harness @ Harness { services, client, client_id, .. }: &Harness<'_>,
	user: &UserId,
	other: &UserId,
) -> Result<String> {
	let grant = services
		.oauth
		.get_server()?
		.create_device_grant(client_id, "openid");

	let response = sign_in(harness, &grant.user_code, USERNAME).await?;
	let cookie = pair(&set_cookie(&response).expect("fresh cookie"));

	consent(harness, &redirect(harness, &response)?).await?;

	let provider = "browser-session-provider-switch";
	let _expires_in =
		services
			.users
			.create_login_token_with_provider(user, provider, Some("first-idp"));

	let foreign = "browser-session-foreign-switch";
	let _expires_in = services.users.create_login_token(other, foreign);

	for token in [provider, foreign] {
		let response = decide(harness, &grant.user_code, token, "switch").await?;

		assert_eq!(response.status(), StatusCode::SEE_OTHER);
	}

	let foreign = "browser-session-foreign-consent";
	let _expires_in = services.users.create_login_token(other, foreign);
	let callback = callback_url(harness, &grant.user_code, foreign)?;
	let response = client.get(callback.as_str()).send().await?;

	assert_eq!(response.status(), StatusCode::FORBIDDEN);

	let page = response.text().await?;

	assert!(page.contains("authenticated by another user or provider"));

	Ok(cookie)
}

/// Turning off any setting the session depends on turns off the session.
///
/// No session is minted or honored while the lifetime is 0, the login page is
/// off or password login is off, and the consent page offers no switch without
/// the login page. Restoring the settings brings the existing session back.
#[tracing::instrument(level = "debug", skip_all)]
async fn settings_turn_sessions_off(
	harness @ Harness { services, .. }: &Harness<'_>,
	user: &UserId,
	cookie: &str,
) -> Result {
	let config = &services.config;
	let original = Config::clone(&services.server.config);
	let code = user_code(harness)?;

	config.replace(with_ttl(original.clone(), 0))?;
	assert_eq!(sign_in_unminted(harness).await?, StatusCode::SEE_OTHER);
	assert_ignored(harness, &code, cookie).await?;

	config.replace(without_native(original.clone()))?;
	assert!(sign_in_unminted(harness).await?.is_client_error());
	assert_ignored(harness, &code, cookie).await?;

	let login_token = "browser-session-no-native";
	let _expires_in = services
		.users
		.create_login_token(user, login_token);

	let callback = callback_url(harness, &code, login_token)?;

	let page = consent(harness, &callback).await?;

	assert!(!page.contains(r#"value="switch""#));

	config.replace(without_password(original.clone()))?;
	assert!(sign_in_unminted(harness).await?.is_client_error());
	assert_ignored(harness, &code, cookie).await?;
	config.replace(original)?;

	assert_honored(harness, &user_code(harness)?, cookie).await
}

/// A password change ends every session of the account.
///
/// It does so whichever password it sets, and other accounts' sessions stay.
#[tracing::instrument(level = "debug", skip_all)]
async fn revocation_ends_sessions(
	harness @ Harness { services, .. }: &Harness<'_>,
	user: &UserId,
	switched: &Switched,
) -> Result {
	let code = user_code(harness)?;

	assert_honored(harness, &code, &switched.cookie).await?;

	services
		.users
		.set_password(user, Some(PASSWORD))
		.await?;

	assert_eq!(sessions(harness, user).await, 0);
	assert_ignored(harness, &code, &switched.cookie).await?;
	assert_honored(harness, &code, &switched.other_cookie).await
}

/// Signing out of every device also signs every browser out.
///
/// A fresh sign-in after the switch adds a second session, and both end.
#[tracing::instrument(level = "debug", skip_all)]
async fn sign_out_everywhere_ends_sessions(
	harness @ Harness { client, base, .. }: &Harness<'_>,
	other: &UserId,
	tokens: &str,
) -> Result {
	let code = user_code(harness)?;
	let response = sign_in(harness, &code, OTHER).await?;
	let cookie = pair(&set_cookie(&response).expect("cookie"));
	let tokens: Value = from_str(tokens)?;
	let access_token = tokens["access_token"]
		.as_str()
		.expect("access token");

	assert_honored(harness, &code, &cookie).await?;

	client
		.post(format!("{base}/_matrix/client/v3/logout/all"))
		.bearer_auth(access_token)
		.send()
		.await?
		.error_for_status()?;

	assert_eq!(sessions(harness, other).await, 0);

	assert_ignored(harness, &code, &cookie).await
}

/// Deactivating an account ends its sessions.
///
/// The row count shows it, since a deactivated account is refused at use
/// whether or not its session survives.
#[tracing::instrument(level = "debug", skip_all)]
async fn deactivation_ends_sessions(harness @ Harness { services, .. }: &Harness<'_>) -> Result {
	let gone = register(services, GONE).await?;
	let response = sign_in(harness, &user_code(harness)?, GONE).await?;

	assert!(set_cookie(&response).is_some());
	assert_eq!(sessions(harness, &gone).await, 1);

	services.users.deactivate_account(&gone).await?;

	assert_eq!(sessions(harness, &gone).await, 0);

	Ok(())
}

/// Where a response redirects, moved onto the test listener.
///
/// The server names the issuer host, which the test cannot reach.
fn redirect(harness: &Harness<'_>, response: &Response) -> Result<Url> {
	let url = location(response)?;
	let query = url
		.query()
		.map(|query| format!("?{query}"))
		.unwrap_or_default();

	Url::parse(&format!("{}{}{query}", harness.base, url.path())).map_err(Into::into)
}

fn callback_url(harness: &Harness<'_>, user_code: &str, login_token: &str) -> Result<Url> {
	let callback = format!("{}/_tuwunel/oidc/device_callback", harness.base);
	let params = [("user_code", user_code), ("loginToken", login_token)];

	Url::parse_with_params(&callback, params).map_err(Into::into)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn register(services: &Services, username: &str) -> Result<OwnedUserId> {
	let user = UserId::parse_with_server_name(username, services.globals.server_name())?;

	services
		.users
		.full_register(Register {
			user_id: Some(&user),
			password: Some(PASSWORD),
			..Default::default()
		})
		.await?;

	Ok(user)
}

fn user_code(harness: &Harness<'_>) -> Result<String> {
	let grant = harness
		.services
		.oauth
		.get_server()?
		.create_device_grant(&harness.client_id, "openid");

	Ok(grant.user_code)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn sign_in(
	Harness { client, base, .. }: &Harness<'_>,
	user_code: &str,
	username: &str,
) -> Result<Response> {
	let fields = [("user_code", user_code), ("username", username), ("password", PASSWORD)];

	post_form(client, base, "native", &fields).await
}

/// Signs in on the password page and asserts no session cookie comes back.
///
/// Returns the status, which differs by why no session was minted.
#[tracing::instrument(level = "debug", skip_all)]
async fn sign_in_unminted(harness: &Harness<'_>) -> Result<StatusCode> {
	let response = sign_in(harness, &user_code(harness)?, USERNAME).await?;

	assert!(set_cookie(&response).is_none());

	Ok(response.status())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn verify(
	Harness { client, base, .. }: &Harness<'_>,
	user_code: &str,
	cookie: Option<&str>,
) -> Result<Response> {
	let request = client
		.get(format!("{base}/_tuwunel/oidc/device"))
		.query(&[("user_code", user_code)]);

	cookie
		.into_iter()
		.fold(request, |request, cookie| request.header(COOKIE, cookie))
		.send()
		.await
		.map_err(Into::into)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn consent(Harness { client, .. }: &Harness<'_>, callback: &Url) -> Result<String> {
	client
		.get(callback.as_str())
		.send()
		.await?
		.error_for_status()?
		.text()
		.await
		.map_err(Into::into)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn decide(
	Harness { client, base, .. }: &Harness<'_>,
	user_code: &str,
	login_token: &str,
	action: &str,
) -> Result<Response> {
	let fields = [("user_code", user_code), ("loginToken", login_token), ("action", action)];

	post_form(client, base, "device_callback", &fields).await
}

/// Polls an approved grant once.
///
/// Returns the token response body, whose tokens later steps use.
#[tracing::instrument(level = "debug", skip_all)]
async fn poll(harness: &Harness<'_>, device_code: &str) -> Result<String> {
	token_request(harness, device_code)
		.await?
		.error_for_status()?
		.text()
		.await
		.map_err(Into::into)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn token_request(
	Harness { client, base, client_id, .. }: &Harness<'_>,
	device_code: &str,
) -> Result<Response> {
	let fields = [
		("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
		("device_code", device_code),
		("client_id", client_id.as_str()),
	];

	post_form(client, base, "token", &fields).await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn whoami(
	Harness { client, base, .. }: &Harness<'_>,
	tokens: &str,
) -> Result<(String, OwnedDeviceId)> {
	let tokens: Value = from_str(tokens)?;
	let access_token = tokens["access_token"]
		.as_str()
		.expect("access token");

	let whoami: Value = client
		.get(format!("{base}/_matrix/client/v3/account/whoami"))
		.bearer_auth(access_token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	let user_id = whoami["user_id"].as_str().expect("user id");
	let device_id = whoami["device_id"].as_str().expect("device id");

	Ok((user_id.to_owned(), device_id.into()))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn assert_honored(harness: &Harness<'_>, user_code: &str, cookie: &str) -> Result {
	let response = verify(harness, user_code, Some(cookie)).await?;

	assert_eq!(response.status(), StatusCode::SEE_OTHER);
	assert_eq!(redirect(harness, &response)?.path(), "/_tuwunel/oidc/device_callback");

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn assert_ignored(harness: &Harness<'_>, user_code: &str, cookie: &str) -> Result {
	let bare = verify(harness, user_code, None).await?;
	let with = verify(harness, user_code, Some(cookie)).await?;

	assert_eq!(with.status(), bare.status());
	assert_eq!(with.headers().get(LOCATION), bare.headers().get(LOCATION));
	assert!(set_cookie(&with).is_none());

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn sessions(Harness { services, .. }: &Harness<'_>, user: &UserId) -> usize {
	services.db[SESSIONS]
		.keys_prefix_raw(&(user, Interfix))
		.count()
		.await
}

/// Every stored session row of a user, to compare across requests.
///
/// The rows are copied out, since a cursor's slices end with its next step.
#[tracing::instrument(level = "debug", skip_all)]
async fn rows(Harness { services, .. }: &Harness<'_>, user: &UserId) -> Result<Vec<Row>> {
	services.db[SESSIONS]
		.stream_prefix_raw(&(user, Interfix))
		.map_ok(|(key, value)| (key.to_vec(), value.to_vec()))
		.try_collect()
		.await
}

/// Asserts a response is neither cached nor named in a referrer.
///
/// Every redirect here carries a user code or a login token in its URL.
fn assert_private(response: &Response) {
	let header = |name| {
		response
			.headers()
			.get(name)
			.and_then(|value| value.to_str().ok())
	};

	assert_eq!(header(CACHE_CONTROL), Some("no-store"));
	assert_eq!(header(REFERRER_POLICY), Some("no-referrer"));
}

fn set_cookie(response: &Response) -> Option<String> {
	response.headers().get(SET_COOKIE).map(|value| {
		value
			.to_str()
			.expect("cookie header text")
			.to_owned()
	})
}

fn pair(header: &str) -> String {
	header
		.split(';')
		.next()
		.expect("cookie pair")
		.to_owned()
}

fn with_ttl(mut config: Config, ttl: u64) -> Config {
	config.authentication.oidc.browser_session_ttl = ttl;
	config
}

fn without_native(mut config: Config) -> Config {
	config.authentication.oidc.native_auth = false;
	config
}

fn without_password(mut config: Config) -> Config {
	config.authentication.login.with_password = false;
	config
}
