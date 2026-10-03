use std::{fmt::Write, net::IpAddr};

use axum::{
	extract::{Form, Request, State},
	response::{Redirect, Response},
};
use const_str::format as const_format;
use http::StatusCode;
use itertools::Either::{Left, Right};
use ruma::{OwnedUserId, UserId};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{
	Err, Error, Result,
	config::IdentityProvider,
	err,
	smallstr::SmallString,
	utils::{self, BoolExt, hash::verify_password, html::escape as html_escape},
};
use tuwunel_service::{Services, users::Register};
use url::Url;

use super::{
	account::{
		ACCOUNT_HEAD, account_error_response, account_html_response, account_redirect_response,
	},
	authorization_sso_url, require_account_usable, url_encode,
};
use crate::ClientIp;

type AccountAction = SmallString<[u8; 32]>;
type DeviceId = SmallString<[u8; 24]>;
type IdpId = SmallString<[u8; 32]>;
type ProviderChoice<'a> = (&'a str, &'a str);

const LOGIN_TOKEN_LENGTH: usize = 32;

#[derive(Debug, Default, Deserialize)]
struct NativeQuery {
	oidc_req_id: Option<String>,
	idp_id: Option<IdpId>,
	user_code: Option<String>,
	action: Option<AccountAction>,
	device_id: Option<DeviceId>,
	view: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct NativeSubmit {
	#[serde(default)]
	oidc_req_id: Option<String>,
	#[serde(default)]
	user_code: Option<String>,
	#[serde(default)]
	action: Option<AccountAction>,
	#[serde(default)]
	device_id: Option<DeviceId>,
	#[serde(default)]
	mode: Option<String>,
	username: String,
	password: String,
	#[serde(default)]
	registration_token: Option<String>,
	#[serde(default)]
	accept_terms: Option<String>,
}

#[derive(Clone, Copy)]
enum Flow<'a> {
	Account {
		action: &'a str,
		device_id: &'a str,
	},
	Authorization(&'a str),
	Device(&'a str),
}

/// Renders the native login or registration page for a pending authorization,
/// device, or account flow.
///
/// A provider chosen on that page arrives here as `idp_id`, and the route
/// redirects to it instead. When the request is already bound to a provider,
/// the page offers only that provider.
pub(crate) async fn native_get_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	request: Request,
) -> Response {
	if let Err(e) = require_native(&services) {
		return account_error_response(&e);
	}

	let params: NativeQuery =
		match serde_html_form::from_str(request.uri().query().unwrap_or_default()) {
			| Ok(params) => params,
			| Err(e) => return account_error_response(&e.into()),
		};

	let context = match parse_flow(
		params.oidc_req_id.as_deref(),
		params.user_code.as_deref(),
		params.action.as_deref(),
		params.device_id.as_deref(),
	) {
		| Ok(context) => context,
		| Err(e) => return account_error_response(&e),
	};

	if let Some(idp_id) = params.idp_id.as_deref() {
		return provider_redirect(&services, client, context, idp_id)
			.await
			.map_or_else(|e| account_error_response(&e), account_redirect_response);
	}

	let view = params.view.as_deref().unwrap_or("login");

	render_page(&services, view, context, None)
		.await
		.map(|html| account_html_response(StatusCode::OK, html))
		.unwrap_or_else(|e| account_error_response(&e))
}

fn parse_flow<'a>(
	oidc_req_id: Option<&'a str>,
	user_code: Option<&'a str>,
	action: Option<&'a str>,
	device_id: Option<&'a str>,
) -> Result<Flow<'a>> {
	match (
		oidc_req_id.filter(|value| !value.is_empty()),
		user_code.filter(|value| !value.is_empty()),
		action.filter(|value| !value.is_empty()),
	) {
		| (Some(req_id), None, None) => Ok(Flow::Authorization(req_id)),
		| (None, Some(user_code), None) => Ok(Flow::Device(user_code)),
		| (None, None, Some(action)) => Ok(Flow::Account {
			action,
			device_id: device_id.unwrap_or_default(),
		}),
		| _ => Err!(Request(InvalidParam(
			"Exactly one OIDC request ID, user code, or account action is required."
		))),
	}
}

/// Send the browser to the provider chosen on the login page.
///
/// The pending request is bound to that provider only once its URL is built, and
/// the binding is final, so the request cannot also complete with a local
/// password or another provider.
async fn provider_redirect(
	services: &Services,
	client: IpAddr,
	context: Flow<'_>,
	idp_id: &str,
) -> Result<Redirect> {
	let Flow::Authorization(req_id) = context else {
		return Err!(Request(InvalidParam(
			"Provider selection requires an authorization request"
		)));
	};

	services.oauth.check_rate_limit(client)?;

	let oidc = services.oauth.get_server()?;
	let provider_id = services
		.oauth
		.providers
		.find_config(idp_id)
		.map_err(|_| err!(Request(InvalidParam("Unrecognized identity provider"))))?
		.id();

	let sso_url = authorization_sso_url(&oidc.issuer_url()?, provider_id, req_id)?;

	oidc.bind_auth_request_to_provider(req_id, provider_id)
		.await?;

	Ok(Redirect::temporary(sso_url.as_str()))
}

/// Authenticates submitted credentials and sends the login token to the
/// authorization completion, device-consent, or account-management callback.
pub(crate) async fn native_submit_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	Form(body): Form<NativeSubmit>,
) -> Response {
	match native_submit(&services, client, &body).await {
		| Ok(response) => response,
		| Err(e) => render_submit_error(&services, &body, &e).await,
	}
}

async fn native_submit(
	services: &Services,
	client: IpAddr,
	body: &NativeSubmit,
) -> Result<Response> {
	require_native(services)?;
	// Always-on anti-brute-force floor; the oidc_rc_* throttle below is opt-in.
	services.oauth.check_device_rate_limit(client)?;
	services.oauth.check_rate_limit(client)?;

	let context = parse_flow(
		body.oidc_req_id.as_deref(),
		body.user_code.as_deref(),
		body.action.as_deref(),
		body.device_id.as_deref(),
	)?;

	let user_id = match context {
		| Flow::Authorization(req_id) => authenticate_local(services, req_id, body).await?,
		| _ => verify_credentials(services, &body.username, &body.password).await?,
	};

	let token = utils::random_string(LOGIN_TOKEN_LENGTH);
	let _expires_in = services
		.users
		.create_login_token(&user_id, &token);

	let redirect = complete_redirect(services, context, &token)?;

	Ok(account_redirect_response(redirect))
}

/// Re-render the page a failed submission came from, carrying its error.
///
/// A submission whose flow cannot be parsed, or whose request has gone, gets
/// the error page instead.
async fn render_submit_error(
	services: &Services,
	body: &NativeSubmit,
	error: &Error,
) -> Response {
	let context = match parse_flow(
		body.oidc_req_id.as_deref(),
		body.user_code.as_deref(),
		body.action.as_deref(),
		body.device_id.as_deref(),
	) {
		| Ok(context) => context,
		| Err(e) => return account_error_response(&e),
	};

	let view = match (context, body.mode.as_deref()) {
		| (Flow::Authorization(_), Some("register")) => "register",
		| _ => "login",
	};

	let msg = error.sanitized_message();

	render_page(services, view, context, Some(&msg))
		.await
		.map(|html| account_html_response(error.status_code(), html))
		.unwrap_or_else(|e| account_error_response(&e))
}

/// Authenticate through the local branch of an authorization request.
///
/// A request bound to a provider is refused before any credential is checked.
/// A login claims the request once the password verifies, and registration
/// claims it before creating the account.
async fn authenticate_local(
	services: &Services,
	req_id: &str,
	body: &NativeSubmit,
) -> Result<OwnedUserId> {
	let oidc = services.oauth.get_server()?;

	oidc.check_local_auth_request(req_id).await?;

	if body.mode.as_deref() == Some("register") {
		return do_register(services, req_id, body).await;
	}

	let user_id = verify_credentials(services, &body.username, &body.password).await?;

	oidc.bind_auth_request_to_local(req_id).await?;

	Ok(user_id)
}

/// Authenticate a local account by password, mirroring the `/login` password
/// flow (`password_login`): password-origin accounts only, uniform error.
async fn verify_credentials(
	services: &Services,
	username: &str,
	password: &str,
) -> Result<OwnedUserId> {
	let invalid = || err!(Request(Forbidden("Invalid username or password.")));
	let server_name = &services.config.server_name;

	let user_id = UserId::parse_with_server_name(username, server_name).map_err(|_| invalid())?;

	if !services.globals.user_is_local(&user_id) {
		return Err(invalid());
	}

	// The same per-account throttle as `/login`, sharing its buckets, so this
	// page is not a second, unthrottled way to guess the same password.
	let reservation = services
		.login_ratelimit
		.reserve_login_attempt(&user_id)?;

	// Native registration lowercases the localpart, so resolve to whichever case
	// carries the password, mirroring `/login`. An unknown account keeps the
	// reservation as a wrong password does.
	let (user_id, hash) = match services.users.password_hash(&user_id).await {
		| Ok(hash) => (user_id, hash),
		| Err(_) => {
			let lowercased = UserId::parse_with_server_name(username.to_lowercase(), server_name)
				.map_err(|_| invalid())?;

			let hash = services
				.users
				.password_hash(&lowercased)
				.await
				.map_err(|_| invalid())?;

			(lowercased, hash)
		},
	};

	// Deactivated accounts, and SSO/LDAP-origin ones that must authenticate
	// through their provider, have no password here to check.
	let unchecked = hash.is_empty()
		|| services
			.users
			.origin(&user_id)
			.await
			.is_ok_and(|origin| origin != "password");

	if unchecked {
		services
			.login_ratelimit
			.refund_login_attempt(reservation)?;

		return Err(invalid());
	}

	verify_password(password, &hash).map_err(|_| invalid())?;

	services
		.login_ratelimit
		.record_login(reservation)?;

	require_account_usable(services, &user_id).await?;

	Ok(user_id)
}

async fn do_register(
	services: &Services,
	req_id: &str,
	body: &NativeSubmit,
) -> Result<OwnedUserId> {
	if !services.config.allow_registration {
		return Err!(Request(Forbidden("Registration is disabled on this server.")));
	}

	let username = body.username.trim().to_lowercase();
	if username.is_empty() {
		return Err!(Request(InvalidUsername("A username is required.")));
	}

	if body.password.is_empty() {
		return Err!(Request(InvalidParam("A password is required.")));
	}

	// This page cannot collect a 3PID, so refuse rather than silently bypass a
	// mandatory-email policy.
	let token_required = services.registration_tokens.is_enabled().await;
	let smtp = &services.config.smtp;
	let email_required = smtp.connection_uri.is_some()
		&& (smtp.require_email_for_registration
			|| (token_required && smtp.require_email_for_token_registration));

	if email_required {
		return Err!(Request(Forbidden(
			"This server requires an email to register, which this page cannot collect."
		)));
	}

	if services
		.config
		.forbidden_usernames
		.is_match(&username)
	{
		return Err!(Request(Forbidden("That username is not allowed.")));
	}

	let user_id = UserId::parse_with_server_name(&username, &services.config.server_name)
		.map_err(|_| err!(Request(InvalidUsername("That username is not valid."))))?;

	user_id.validate_strict().map_err(|_| {
		err!(Request(InvalidUsername("That username contains disallowed characters.")))
	})?;

	if services
		.appservice
		.is_exclusive_user_id(&user_id)
		.await
	{
		return Err!(Request(Exclusive("That username is reserved by an appservice.")));
	}

	if services.users.exists(&user_id).await {
		return Err!(Request(UserInUse("That username is taken.")));
	}

	services.users.check_creation(&user_id).await?;

	// Acceptance is checked before any token is consumed, so a missing checkbox
	// does not burn a single-use registration token.
	if !services.config.registration_terms.is_empty()
		&& body.accept_terms.as_deref() != Some("on")
	{
		return Err!(Request(Forbidden("You must accept the terms to register.")));
	}

	let token = body
		.registration_token
		.as_deref()
		.unwrap_or_default();

	// Validate before claiming, so a mistyped token leaves provider choice open.
	if token_required {
		services
			.registration_tokens
			.is_token_valid(token)
			.await?;
	}

	// Claim this branch before consuming the token or creating the account.
	services
		.oauth
		.get_server()?
		.bind_auth_request_to_local(req_id)
		.await?;

	if token_required {
		services
			.registration_tokens
			.try_consume(token)
			.await?;
	}

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some(&body.password),
			grant_first_user_admin: true,
			..Default::default()
		})
		.await?;

	record_accepted_terms(services, &user_id).await?;

	Ok(user_id)
}

async fn record_accepted_terms(services: &Services, user_id: &UserId) -> Result {
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

/// Redirects with 303 so the browser cannot replay the password form into the
/// completion or callback route.
fn complete_redirect(services: &Services, flow: Flow<'_>, login_token: &str) -> Result<Redirect> {
	let issuer = services.oauth.get_server()?.issuer_url()?;
	let base = issuer.trim_end_matches('/');

	let url = match flow {
		| Flow::Device(user_code) =>
			Url::parse_with_params(&format!("{base}/_tuwunel/oidc/device_callback"), [
				("user_code", user_code),
				("loginToken", login_token),
			]),
		| Flow::Authorization(req_id) =>
			Url::parse_with_params(&format!("{base}/_tuwunel/oidc/_complete"), [
				("oidc_req_id", req_id),
				("loginToken", login_token),
			]),
		| Flow::Account { action, device_id } =>
			Url::parse_with_params(&format!("{base}/_tuwunel/oidc/account_callback"), [
				("action", action),
				("device_id", device_id),
				("loginToken", login_token),
			]),
	}
	.map_err(|_| err!(error!("Failed to build completion URL")))?;

	Ok(Redirect::to(url.as_str()))
}

fn require_native(services: &Services) -> Result {
	services.oauth.get_server()?;

	services
		.config
		.oidc_native_auth
		.then_some(())
		.ok_or_else(|| err!(Request(NotFound("Native authentication is not enabled"))))
}

/// Render the page for a flow, reading an authorization request's binding.
///
/// A request bound to a provider offers only that provider, whatever view was
/// asked for. An unknown or expired request is an error rather than a page.
async fn render_page(
	services: &Services,
	view: &str,
	context: Flow<'_>,
	error: Option<&str>,
) -> Result<String> {
	let registration_enabled = services.config.allow_registration;
	let Flow::Authorization(req_id) = context else {
		return Ok(render_login(context, error, registration_enabled, ""));
	};

	let bound = services
		.oauth
		.get_server()?
		.peek_auth_request(req_id)
		.await?
		.idp_id;

	let page = match bound.as_deref() {
		| None if view == "register" && registration_enabled =>
			render_register(services, req_id, error).await,

		| Some(idp_id) => {
			let provider = services.oauth.providers.find_config(idp_id)?;

			render_bound(req_id, provider_choice(provider), error)
		},

		| None => {
			let sso_options =
				render_sso_options("Or sign in with", req_id, sso_choices(services));

			render_login(context, error, registration_enabled, &sso_options)
		},
	};

	Ok(page)
}

fn render_login(
	context: Flow<'_>,
	error: Option<&str>,
	show_register: bool,
	sso_options: &str,
) -> String {
	let (context_fields, register_link) = match context {
		| Flow::Device(user_code) => {
			let context_fields = format!(
				r#"<input type="hidden" name="user_code" value="{}">"#,
				html_escape(user_code),
			);

			(context_fields, String::new())
		},
		| Flow::Account { action, device_id } => {
			let context_fields = format!(
				concat!(
					r#"<input type="hidden" name="action" value="{}">"#,
					"\n\t\t\t",
					r#"<input type="hidden" name="device_id" value="{}">"#,
				),
				html_escape(action),
				html_escape(device_id),
			);

			(context_fields, String::new())
		},
		| Flow::Authorization(req_id) => {
			let context_fields = format!(
				r#"<input type="hidden" name="oidc_req_id" value="{}">"#,
				html_escape(req_id),
			);

			let register_link = show_register
				.then(|| {
					format!(
						r#"<p class="auth-nav">New to this server? <a href="/_tuwunel/oidc/native?oidc_req_id={}&amp;view=register">Create an account</a></p>"#,
						url_encode(req_id),
					)
				})
				.unwrap_or_default();

			(context_fields, register_link)
		},
	};

	LOGIN_HTML
		.replace("{register_link}", &register_link)
		.replace("{sso_options}", sso_options)
		.replace("{error}", &error_block(error))
		// Fill caller-supplied fields last so they cannot smuggle a placeholder.
		.replace("{context_fields}", &context_fields)
}

async fn render_register(services: &Services, req_id: &str, error: Option<&str>) -> String {
	let token_field = services
		.registration_tokens
		.is_enabled()
		.await
		.then_some(TOKEN_FIELD)
		.unwrap_or_default();

	REGISTER_HTML
		.replace("{token_field}", token_field)
		.replace("{req_id_enc}", &url_encode(req_id))
		.replace("{terms}", &terms_block(services))
		.replace("{error}", &error_block(error))
		// Fill the caller-supplied {req_id} last so it cannot smuggle a placeholder.
		.replace("{req_id}", &html_escape(req_id))
}

fn provider_choice(provider: &IdentityProvider) -> ProviderChoice<'_> {
	(provider.id(), provider.display_name())
}

/// Offer only the provider a pending request is bound to.
///
/// A user who leaves that provider before finishing returns here, and the
/// request can complete only through it.
fn render_bound(req_id: &str, provider: ProviderChoice<'_>, error: Option<&str>) -> String {
	let sso_options = render_sso_options("Continue with", req_id, [provider]);

	BOUND_HTML
		.replace("{sso_options}", &sso_options)
		.replace("{error}", &error_block(error))
}

/// Providers the login page offers, as the client login flows list them.
///
/// When `single_sso` or `sso_custom_providers_page` replaces that list, the page
/// offers one single sign-on entry for the default provider instead.
fn sso_choices(services: &Services) -> impl Iterator<Item = ProviderChoice<'_>> {
	let listed = services
		.config
		.identity_provider
		.values()
		.map(provider_choice);

	let single = services
		.oauth
		.providers
		.find_default_config()
		.map(|provider| (provider.id(), "Single sign-on"));

	match services.config.lists_identity_providers() {
		| true => Left(listed),
		| false => Right(single.into_iter()),
	}
}

/// List each provider as a link that binds the pending request to it.
///
/// Names are HTML-escaped with braces encoded too, since the result is filled in
/// before the error and context placeholders.
fn render_sso_options<'a, I>(heading: &str, req_id: &str, providers: I) -> String
where
	I: IntoIterator<Item = ProviderChoice<'a>>,
{
	let req_id = url_encode(req_id);
	let options = providers
		.into_iter()
		.map(|(id, name)| {
			let name = html_escape(name)
				.replace('{', "&#123;")
				.replace('}', "&#125;");

			(url_encode(id), name)
		})
		.fold(String::new(), |mut out, (id, name)| {
			write!(
				out,
				r#"<li><a href="/_tuwunel/oidc/native?oidc_req_id={req_id}&amp;idp_id={id}">{name}</a></li>"#,
			)
			.ok();

			out
		});

	options
		.is_empty()
		.is_false()
		.then(|| {
			format!(
				r#"<section class="sso-options"><h2>{heading}</h2><ul>{options}</ul></section>"#
			)
		})
		.unwrap_or_default()
}

fn error_block(error: Option<&str>) -> String {
	error
		.map(|msg| format!(r#"<p class="err">{}</p>"#, html_escape(msg)))
		.unwrap_or_default()
}

fn terms_block(services: &Services) -> String {
	let policies = &services.config.registration_terms;
	if policies.is_empty() {
		return String::new();
	}

	let links = policies
		.values()
		.filter_map(|policy| {
			policy
				.translations
				.get("en")
				.or_else(|| policy.translations.values().next())
		})
		.fold(String::new(), |mut links, translation| {
			write!(
				links,
				r#"<li><a href="{}" target="_blank" rel="noopener noreferrer">{}</a></li>"#,
				html_escape(translation.url.as_str()),
				html_escape(&translation.name),
			)
			.ok();

			links
		});

	format!(
		r#"<fieldset class="terms"><legend>Terms</legend><ul>{links}</ul><label><input type="checkbox" name="accept_terms" value="on" required> I accept the terms above.</label></fieldset>"#
	)
}

static LOGIN_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Sign in · Tuwunel</title>
	</head>
	<body class="auth-page">
		<main class="auth-card" aria-labelledby="auth-title">
			<h1 id="auth-title">Sign in</h1>
			<p class="auth-description">Sign in to your Tuwunel account.</p>
			{{error}}
			<form class="auth-form" method="POST" action="/_tuwunel/oidc/native">
				{{context_fields}}
				<input type="hidden" name="mode" value="login">
				<label for="auth-username">Username</label>
				<input id="auth-username" type="text" name="username" autocomplete="username" autofocus required>
				<label for="auth-password">Password</label>
				<input id="auth-password" type="password" name="password" autocomplete="current-password" required>
				<button type="submit">Sign in</button>
			</form>
			{{sso_options}}
			{{register_link}}
		</main>
	</body>
</html>"#
);

static REGISTER_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Create account · Tuwunel</title>
	</head>
	<body class="auth-page">
		<main class="auth-card" aria-labelledby="auth-title">
			<h1 id="auth-title">Create account</h1>
			<p class="auth-description">Set up your account on this homeserver.</p>
			{{error}}
			<form class="auth-form" method="POST" action="/_tuwunel/oidc/native">
				<input type="hidden" name="oidc_req_id" value="{{req_id}}">
				<input type="hidden" name="mode" value="register">
				<label for="auth-username">Username</label>
				<input id="auth-username" type="text" name="username" autocomplete="username" autofocus required>
				<label for="auth-password">Password</label>
				<input id="auth-password" type="password" name="password" autocomplete="new-password" required>
				{{token_field}}
				{{terms}}
				<button type="submit">Create account</button>
			</form>
			<p class="auth-nav">Already have an account? <a href="/_tuwunel/oidc/native?oidc_req_id={{req_id_enc}}&amp;view=login">Sign in</a></p>
		</main>
	</body>
</html>"#
);

static BOUND_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Continue signing in · Tuwunel</title>
	</head>
	<body class="auth-page">
		<main class="auth-card" aria-labelledby="auth-title">
			<h1 id="auth-title">Continue signing in</h1>
			<p class="auth-description">Finish signing in with the provider you chose.</p>
			{{error}}
			{{sso_options}}
		</main>
	</body>
</html>"#
);

static TOKEN_FIELD: &str = r#"<label for="auth-token">Registration token</label>
				<input id="auth-token" type="text" name="registration_token" autocomplete="off" placeholder="Enter your token" required>"#;

#[cfg(test)]
mod tests {
	use super::{Flow, error_block, parse_flow, render_bound, render_login, render_sso_options};

	#[test]
	fn login_page_has_form_and_hidden_req_id() {
		let html = render_login(Flow::Authorization("REQ123"), None, false, "");

		assert!(html.contains(r#"action="/_tuwunel/oidc/native""#));
		assert!(html.contains(r#"name="oidc_req_id" value="REQ123""#));
		assert!(html.contains(r#"name="username""#));
		assert!(html.contains(r#"name="password""#));
		assert!(!html.contains("view=register"));
	}

	#[test]
	fn login_page_links_to_register_when_enabled() {
		let html = render_login(Flow::Authorization("REQ123"), None, true, "");

		assert!(html.contains("oidc_req_id=REQ123&amp;view=register"));
	}

	#[test]
	fn login_page_offers_each_provider_with_a_bound_request() {
		let providers =
			[("first/provider", "First provider"), ("second", "Second {error} <provider>")];

		let options = render_sso_options("Or sign in with", "REQ123", providers);
		let html = render_login(Flow::Authorization("REQ123"), None, false, &options);

		assert!(html.contains("oidc_req_id=REQ123&amp;idp_id=first%2Fprovider"));
		assert!(html.contains("oidc_req_id=REQ123&amp;idp_id=second"));
		assert!(html.contains("Second &#123;error&#125; &lt;provider&gt;"));
		assert!(!html.contains("<provider>"));
		assert!(html.contains(r#"name="password""#));
	}

	#[test]
	fn bound_page_offers_only_its_provider() {
		let html = render_bound("REQ123", ("first", "First <provider>"), Some("Already chosen"));

		assert!(html.contains("oidc_req_id=REQ123&amp;idp_id=first"));
		assert!(html.contains("First &lt;provider&gt;"));
		assert!(html.contains("Already chosen"));
		assert!(!html.contains(r#"name="password""#));
		assert!(!html.contains("{sso_options}"));
	}

	#[test]
	fn login_page_escapes_error_and_req_id() {
		let html = render_login(
			Flow::Authorization("a<b>c"),
			Some("<script>alert(1)</script>"),
			false,
			"",
		);

		assert!(!html.contains("<script>"));
		assert!(html.contains("&lt;script&gt;"));
		assert!(!html.contains("a<b>c"));
		assert!(html.contains("a&lt;b&gt;c"));
	}

	#[test]
	fn login_page_does_not_expand_smuggled_placeholder() {
		// A req_id of "{error}" must not be re-expanded by the later error fill.
		let html = render_login(Flow::Authorization("{error}"), Some("BOOM"), false, "");

		assert_eq!(html.matches("BOOM").count(), 1);
		assert!(html.contains(r#"value="{error}""#));
	}

	#[test]
	fn device_login_page_has_only_hidden_user_code() {
		let html = render_login(Flow::Device("BCDF-GHJK"), None, true, "");

		assert!(html.contains(r#"name="user_code" value="BCDF-GHJK""#));
		assert!(!html.contains(r#"name="oidc_req_id""#));
		assert!(!html.contains("view=register"));
	}

	#[test]
	fn device_login_page_escapes_and_does_not_expand_context() {
		let html = render_login(Flow::Device("a<{error}>"), Some("BOOM"), true, "");

		assert_eq!(html.matches("BOOM").count(), 1);
		assert!(!html.contains("a<{error}>"));
		assert!(html.contains(r#"value="a&lt;{error}&gt;""#));
	}

	#[test]
	fn account_login_page_has_hidden_action_and_device_id() {
		let context = Flow::Account {
			action: "org.matrix.sessions_list",
			device_id: "",
		};

		let html = render_login(context, None, true, "");

		assert!(html.contains(r#"name="action" value="org.matrix.sessions_list""#));
		assert!(html.contains(r#"name="device_id" value="""#));
		assert!(!html.contains(r#"name="oidc_req_id""#));
		assert!(!html.contains(r#"name="user_code""#));
		assert!(!html.contains("view=register"));
	}

	#[test]
	fn account_login_page_escapes_and_does_not_expand_context() {
		let context = Flow::Account {
			action: "a<{error}>",
			device_id: "b<{error}>",
		};

		let html = render_login(context, Some("BOOM"), true, "");

		assert_eq!(html.matches("BOOM").count(), 1);
		assert!(!html.contains("a<{error}>"));
		assert!(!html.contains("b<{error}>"));
		assert!(html.contains(r#"name="action" value="a&lt;{error}&gt;""#));
		assert!(html.contains(r#"name="device_id" value="b&lt;{error}&gt;""#));
	}

	#[test]
	fn flow_requires_exactly_one_nonempty_value() {
		assert!(matches!(
			parse_flow(Some("REQ123"), None, None, None),
			Ok(Flow::Authorization("REQ123"))
		));

		assert!(matches!(
			parse_flow(None, Some("BCDF-GHJK"), None, None),
			Ok(Flow::Device("BCDF-GHJK"))
		));

		assert!(matches!(
			parse_flow(None, None, Some("org.matrix.sessions_list"), None),
			Ok(Flow::Account {
				action: "org.matrix.sessions_list",
				device_id: "",
			})
		));

		assert!(matches!(
			parse_flow(None, None, Some("org.matrix.session_view"), Some("DEVICE")),
			Ok(Flow::Account {
				action: "org.matrix.session_view",
				device_id: "DEVICE",
			})
		));

		assert!(matches!(
			parse_flow(None, None, Some("org.matrix.sessions_list"), Some("")),
			Ok(Flow::Account {
				action: "org.matrix.sessions_list",
				device_id: "",
			})
		));

		assert!(parse_flow(None, None, None, None).is_err());
		assert!(parse_flow(None, None, None, Some("DEVICE")).is_err());
		assert!(parse_flow(Some(""), None, None, None).is_err());
		assert!(parse_flow(None, Some(""), None, None).is_err());
		assert!(parse_flow(None, None, Some(""), None).is_err());
		assert!(parse_flow(None, None, Some(""), Some("DEVICE")).is_err());
		assert!(parse_flow(Some("REQ123"), Some("BCDF-GHJK"), None, None).is_err());
		assert!(
			parse_flow(Some("REQ123"), None, Some("org.matrix.sessions_list"), None).is_err()
		);

		assert!(
			parse_flow(None, Some("BCDF-GHJK"), Some("org.matrix.sessions_list"), None).is_err()
		);

		assert!(
			parse_flow(
				Some("REQ123"),
				Some("BCDF-GHJK"),
				Some("org.matrix.sessions_list"),
				None,
			)
			.is_err()
		);
	}

	#[test]
	fn error_block_renders_only_when_present() {
		let block = error_block(None);

		assert!(block.is_empty(), "{block:?}");
		assert!(error_block(Some("oops")).contains(r#"class="err""#));
	}
}
