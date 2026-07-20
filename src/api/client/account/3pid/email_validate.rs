use std::net::IpAddr;

use axum::{
	extract::{Form, Request, State},
	response::{Html, IntoResponse, Response},
};
use const_str::format as const_format;
use http::{
	StatusCode,
	header::{CACHE_CONTROL, REFERRER_POLICY},
};
use serde::Deserialize;
use tuwunel_core::utils::html::escape as html_escape;

use crate::ClientIp;

static VALIDATE_HEAD: &str = r#"
	<meta charset="UTF-8">
	<link rel="stylesheet" href="/_tuwunel/oidc/account.css">
"#;

static GENERIC_FAILURE: &str =
	"This verification link is invalid or has expired. Request a new one from your client.";

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ValidateParams {
	sid: Option<String>,
	client_secret: Option<String>,
	token: Option<String>,
}

/// # `GET /_tuwunel/3pid/email/validate`
///
/// The magic-link target. Renders a confirmation page whose form posts the same
/// parameters back; the token is never consumed on this request, so an email
/// scanner that prefetches the link cannot spend it.
pub(crate) async fn get_email_validate_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	request: Request,
) -> Response {
	if let Some(limited) = rate_limited(services, client) {
		return limited;
	}

	let params: ValidateParams =
		serde_html_form::from_str(request.uri().query().unwrap_or_default()).unwrap_or_default();

	validate_html(StatusCode::OK, confirm_html(&params))
}

/// # `POST /_tuwunel/3pid/email/validate`
///
/// Confirms the validation. A wrong or expired session renders the same failure
/// page as any other error, so the page never reveals whether a session or
/// token is live.
pub(crate) async fn post_email_validate_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	Form(params): Form<ValidateParams>,
) -> Response {
	if let Some(limited) = rate_limited(services, client) {
		return limited;
	}

	let (Some(sid), Some(client_secret), Some(token)) =
		(&params.sid, &params.client_secret, &params.token)
	else {
		return validate_html(StatusCode::OK, error_html(GENERIC_FAILURE));
	};

	match services
		.threepid
		.validate_pending_token(sid, client_secret, token)
		.await
	{
		| Ok(()) => validate_html(
			StatusCode::OK,
			result_html(
				"Email verified",
				"Your email address has been verified. Return to your client to continue.",
			),
		),
		| Err(_) => validate_html(StatusCode::OK, error_html(GENERIC_FAILURE)),
	}
}

fn rate_limited(services: crate::State, client: IpAddr) -> Option<Response> {
	services
		.threepid
		.check_ip_rate_limit(client)
		.is_err()
		.then(|| {
			validate_html(
				StatusCode::TOO_MANY_REQUESTS,
				error_html("Too many requests. Please wait and try again."),
			)
		})
}

fn confirm_html(params: &ValidateParams) -> String {
	let escape = |value: &Option<String>| html_escape(value.as_deref().unwrap_or_default());

	// Token first: a later replace must not refill an injected {token}.
	CONFIRM_HTML
		.replace("{token}", &escape(&params.token))
		.replace("{client_secret}", &escape(&params.client_secret))
		.replace("{sid}", &escape(&params.sid))
}

static CONFIRM_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{VALIDATE_HEAD}
		<title>Verify your email address</title>
	</head>
	<body>
		<h1>Verify your email address</h1>
		<p>Confirm that you want to verify this email address.</p>
		<form method="POST" action="/_tuwunel/3pid/email/validate">
			<input type="hidden" name="sid" value="{{sid}}">
			<input type="hidden" name="client_secret" value="{{client_secret}}">
			<input type="hidden" name="token" value="{{token}}">
			<button type="submit" class="primary">Verify</button>
		</form>
	</body>
</html>"#
);

fn result_html(title: &str, message: &str) -> String {
	RESULT_HTML
		.replace("{title}", &html_escape(title))
		.replace("{message}", &html_escape(message))
}

static RESULT_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{VALIDATE_HEAD}
		<title>{{title}}</title>
	</head>
	<body>
		<h1>{{title}}</h1>
		<p>{{message}}</p>
	</body>
</html>"#
);

fn error_html(message: &str) -> String { ERROR_HTML.replace("{msg}", &html_escape(message)) }

static ERROR_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{VALIDATE_HEAD}
		<title>Verification failed</title>
	</head>
	<body>
		<h1 class="err">Verification failed</h1>
		<p>{{msg}}</p>
	</body>
</html>"#
);

fn validate_html(status: StatusCode, html: String) -> Response {
	let headers = [(CACHE_CONTROL, "no-store"), (REFERRER_POLICY, "no-referrer")];

	(status, headers, Html(html)).into_response()
}
