use std::{fmt::Write, net::IpAddr};

use axum::{
	extract::{Form, Request, State},
	response::{Redirect, Response},
};
use const_str::format as const_format;
use http::StatusCode;
use ruma::{OwnedUserId, UserId};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{
	Err, Result, err,
	utils::{self, hash, html::escape as html_escape},
};
use tuwunel_service::{Services, users::Register};
use url::Url;

use super::{
	account::{
		ACCOUNT_HEAD, account_error_response, account_html_response, account_redirect_response,
	},
	url_encode,
};
use crate::ClientIp;

const LOGIN_TOKEN_LENGTH: usize = 32;

#[derive(Debug, Default, Deserialize)]
struct NativeQuery {
	oidc_req_id: Option<String>,
	user_code: Option<String>,
	view: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct NativeSubmit {
	#[serde(default)]
	oidc_req_id: Option<String>,
	#[serde(default)]
	user_code: Option<String>,
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
	Authorization(&'a str),
	Device(&'a str),
}

/// Renders the native login or registration page bound to a pending
/// authorization request.
pub(crate) async fn native_get_route(
	State(services): State<crate::State>,
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

	let context = match parse_flow(params.oidc_req_id.as_deref(), params.user_code.as_deref()) {
		| Ok(context) => context,
		| Err(e) => return account_error_response(&e),
	};

	let view = params.view.as_deref().unwrap_or("login");

	account_html_response(StatusCode::OK, render_page(&services, view, context, None).await)
}

fn parse_flow<'a>(oidc_req_id: Option<&'a str>, user_code: Option<&'a str>) -> Result<Flow<'a>> {
	match (
		oidc_req_id.filter(|value| !value.is_empty()),
		user_code.filter(|value| !value.is_empty()),
	) {
		| (Some(req_id), None) => Ok(Flow::Authorization(req_id)),
		| (None, Some(user_code)) => Ok(Flow::Device(user_code)),
		| _ =>
			Err!(Request(InvalidParam("Exactly one OIDC request ID or user code is required."))),
	}
}

/// Authenticates submitted credentials and hands the resulting login token to
/// the authorization-code completion or device-consent callback.
pub(crate) async fn native_submit_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	Form(body): Form<NativeSubmit>,
) -> Response {
	match native_submit(&services, client, &body).await {
		| Ok(response) => response,
		| Err(e) => {
			let context = match parse_flow(body.oidc_req_id.as_deref(), body.user_code.as_deref())
			{
				| Ok(context) => context,
				| Err(context_error) => return account_error_response(&context_error),
			};

			let view = match (context, body.mode.as_deref()) {
				| (Flow::Authorization(_), Some("register")) => "register",
				| _ => "login",
			};

			let msg = e.sanitized_message();
			let html = render_page(&services, view, context, Some(&msg)).await;

			account_html_response(e.status_code(), html)
		},
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

	let context = parse_flow(body.oidc_req_id.as_deref(), body.user_code.as_deref())?;
	let user_id = match (context, body.mode.as_deref()) {
		| (Flow::Authorization(_), Some("register")) => do_register(services, body).await?,
		| _ => verify_credentials(services, &body.username, &body.password).await?,
	};

	let token = utils::random_string(LOGIN_TOKEN_LENGTH);
	let _expires_in = services
		.users
		.create_login_token(&user_id, &token);

	let redirect = complete_redirect(services, context, &token)?;

	Ok(account_redirect_response(redirect))
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

	// Native registration lowercases the localpart, so resolve to whichever case
	// carries the password, mirroring `/login`.
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

	// SSO/LDAP-origin accounts must authenticate through their provider.
	if services
		.users
		.origin(&user_id)
		.await
		.is_ok_and(|origin| origin != "password")
	{
		return Err(invalid());
	}

	if hash.is_empty() {
		return Err(invalid());
	}

	hash::verify_password(password, &hash).map_err(|_| invalid())?;

	Ok(user_id)
}

async fn do_register(services: &Services, body: &NativeSubmit) -> Result<OwnedUserId> {
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

	// Acceptance is checked before any token is consumed, so a missing checkbox
	// does not burn a single-use registration token.
	if !services.config.registration_terms.is_empty()
		&& body.accept_terms.as_deref() != Some("on")
	{
		return Err!(Request(Forbidden("You must accept the terms to register.")));
	}

	if token_required {
		let token = body
			.registration_token
			.as_deref()
			.unwrap_or_default();

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
	let (path, context_name, context_value) = match flow {
		| Flow::Device(user_code) => ("/_tuwunel/oidc/device_callback", "user_code", user_code),
		| Flow::Authorization(req_id) => ("/_tuwunel/oidc/_complete", "oidc_req_id", req_id),
	};

	let url = Url::parse(&format!("{base}{path}"))
		.map_err(|_| err!(error!("Failed to build completion URL")))
		.map(|mut url| {
			url.query_pairs_mut()
				.append_pair(context_name, context_value)
				.append_pair("loginToken", login_token);

			url
		})?;

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

async fn render_page(
	services: &Services,
	view: &str,
	context: Flow<'_>,
	error: Option<&str>,
) -> String {
	let registration_enabled = services.config.allow_registration;

	match (context, view) {
		| (Flow::Authorization(req_id), "register") if registration_enabled =>
			render_register(services, req_id, error).await,
		| _ => render_login(context, error, registration_enabled),
	}
}

fn render_login(context: Flow<'_>, error: Option<&str>, show_register: bool) -> String {
	let (context_name, context_value, register_link) = match context {
		| Flow::Device(user_code) => ("user_code", user_code, String::new()),
		| Flow::Authorization(req_id) => {
			let register_link = show_register
				.then(|| {
					format!(
						r#"<p class="nav">No account? <a href="/_tuwunel/oidc/native?oidc_req_id={}&amp;view=register">Create one</a>.</p>"#,
						url_encode(req_id),
					)
				})
				.unwrap_or_default();

			("oidc_req_id", req_id, register_link)
		},
	};

	LOGIN_HTML
		.replace("{register_link}", &register_link)
		.replace("{error}", &error_block(error))
		.replace("{context_name}", context_name)
		// Fill the caller-supplied context last so it cannot smuggle a placeholder.
		.replace("{context_value}", &html_escape(context_value))
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
		<title>Sign In</title>
	</head>
	<body>
		<h1>Sign In</h1>
		{{error}}
		<form method="POST" action="/_tuwunel/oidc/native">
			<input type="hidden" name="{{context_name}}" value="{{context_value}}">
			<input type="hidden" name="mode" value="login">
			<label>
				Username
				<input type="text" name="username" autocomplete="username" autofocus required>
			</label>
			<label>
				Password
				<input type="password" name="password" autocomplete="current-password" required>
			</label>
			<button type="submit">Sign in</button>
		</form>
		{{register_link}}
	</body>
</html>"#
);

static REGISTER_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Create Account</title>
	</head>
	<body>
		<h1>Create Account</h1>
		{{error}}
		<form method="POST" action="/_tuwunel/oidc/native">
			<input type="hidden" name="oidc_req_id" value="{{req_id}}">
			<input type="hidden" name="mode" value="register">
			<label>
				Username
				<input type="text" name="username" autocomplete="username" autofocus required>
			</label>
			<label>
				Password
				<input type="password" name="password" autocomplete="new-password" required>
			</label>
			{{token_field}}
			{{terms}}
			<button type="submit">Create account</button>
		</form>
		<p class="nav">Have an account? <a href="/_tuwunel/oidc/native?oidc_req_id={{req_id_enc}}&amp;view=login">Sign in</a>.</p>
	</body>
</html>"#
);

static TOKEN_FIELD: &str = r#"<label>
				Registration token
				<input type="text" name="registration_token" autocomplete="off" required>
			</label>"#;

#[cfg(test)]
mod tests {
	use super::{Flow, error_block, parse_flow, render_login};

	#[test]
	fn login_page_has_form_and_hidden_req_id() {
		let html = render_login(Flow::Authorization("REQ123"), None, false);

		assert!(html.contains(r#"action="/_tuwunel/oidc/native""#));
		assert!(html.contains(r#"name="oidc_req_id" value="REQ123""#));
		assert!(html.contains(r#"name="username""#));
		assert!(html.contains(r#"name="password""#));
		assert!(!html.contains("view=register"));
	}

	#[test]
	fn login_page_links_to_register_when_enabled() {
		let html = render_login(Flow::Authorization("REQ123"), None, true);

		assert!(html.contains("oidc_req_id=REQ123&amp;view=register"));
	}

	#[test]
	fn login_page_escapes_error_and_req_id() {
		let html =
			render_login(Flow::Authorization("a<b>c"), Some("<script>alert(1)</script>"), false);

		assert!(!html.contains("<script>"));
		assert!(html.contains("&lt;script&gt;"));
		assert!(!html.contains("a<b>c"));
		assert!(html.contains("a&lt;b&gt;c"));
	}

	#[test]
	fn login_page_does_not_expand_smuggled_placeholder() {
		// A req_id of "{error}" must not be re-expanded by the later error fill.
		let html = render_login(Flow::Authorization("{error}"), Some("BOOM"), false);

		assert_eq!(html.matches("BOOM").count(), 1);
		assert!(html.contains(r#"value="{error}""#));
	}

	#[test]
	fn device_login_page_has_only_hidden_user_code() {
		let html = render_login(Flow::Device("BCDF-GHJK"), None, true);

		assert!(html.contains(r#"name="user_code" value="BCDF-GHJK""#));
		assert!(!html.contains(r#"name="oidc_req_id""#));
		assert!(!html.contains("view=register"));
	}

	#[test]
	fn device_login_page_escapes_and_does_not_expand_context() {
		let html = render_login(Flow::Device("a<{error}>"), Some("BOOM"), true);

		assert_eq!(html.matches("BOOM").count(), 1);
		assert!(!html.contains("a<{error}>"));
		assert!(html.contains(r#"value="a&lt;{error}&gt;""#));
	}

	#[test]
	fn flow_requires_exactly_one_nonempty_value() {
		assert!(matches!(parse_flow(Some("REQ123"), None), Ok(Flow::Authorization("REQ123"))));

		assert!(matches!(parse_flow(None, Some("BCDF-GHJK")), Ok(Flow::Device("BCDF-GHJK"))));

		assert!(parse_flow(None, None).is_err());
		assert!(parse_flow(Some(""), None).is_err());
		assert!(parse_flow(None, Some("")).is_err());
		assert!(parse_flow(Some("REQ123"), Some("BCDF-GHJK")).is_err());
	}

	#[test]
	fn error_block_renders_only_when_present() {
		assert!(error_block(None).is_empty());
		assert!(error_block(Some("oops")).contains(r#"class="err""#));
	}
}
