use std::{iter::once, net::IpAddr};

use axum::{
	Form,
	extract::State,
	response::{IntoResponse, Redirect, Response},
};
use const_str::format as const_format;
use http::StatusCode;
use ruma::UserId;
use serde::Deserialize;
use tuwunel_core::{Result, err, utils::html::escape as html_escape};
use tuwunel_service::{Services, oauth::server::AuthRequest};
use url::{Url, form_urlencoded};

use super::{
	account::{ACCOUNT_HEAD, account_error_response, account_html_response},
	consume_login_token, peek_login_token, redirect_allowlisted,
};
use crate::ClientIp;

#[derive(Debug, Deserialize)]
pub(crate) struct CompleteParams {
	oidc_req_id: String,

	#[serde(rename = "loginToken")]
	login_token: String,

	/// The button the user pressed on the approval form.
	///
	/// Absent on the authentication provider's return leg, which is a GET and
	/// carries no form at all.
	#[serde(default)]
	action: Option<String>,
}

struct Approval<'a> {
	user_id: &'a UserId,
	client_name: &'a str,
	client_uri: &'a str,
	redirect_uri: &'a str,
	scope: &'a str,
	req_id: &'a str,
	login_token: &'a str,
}

static DENIED_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Sign-in refused</title>
	</head>
	<body>
		<h1>Sign-in refused</h1>
		<p>Nothing was shared with the application. You can close this page.</p>
	</body>
</html>"#
);

pub(crate) async fn complete_route(
	State(services): State<crate::State>,
	request: axum::extract::Request,
) -> Result<Response> {
	let query = request.uri().query().unwrap_or_default();
	let params: CompleteParams = serde_html_form::from_str(query)?;

	let oidc = services.oauth.get_server()?;

	// Validate the auth request first (before consuming the login_token) so that
	// a crafted request with an invalid oidc_req_id cannot burn a valid token.
	let auth_req = oidc
		.peek_auth_request(&params.oidc_req_id)
		.await?;

	if approval_waived(&services, &auth_req.redirect_uri) {
		return release_code(&services, &auth_req, &params).await;
	}

	let user_id = peek_login_token(&services, Some(&params.login_token)).await?;
	let client = oidc.get_client(&auth_req.client_id).await.ok();

	let name = client
		.as_ref()
		.and_then(|client| client.client_name.as_deref())
		.unwrap_or(&auth_req.client_id);

	let website = client
		.as_ref()
		.and_then(|client| client.client_uri.as_deref())
		.unwrap_or(&auth_req.redirect_uri);

	let approval = Approval {
		user_id: &user_id,
		client_name: name,
		client_uri: website,
		redirect_uri: &auth_req.redirect_uri,
		scope: &auth_req.scope,
		req_id: &params.oidc_req_id,
		login_token: &params.login_token,
	};

	Ok(account_html_response(StatusCode::OK, approval.render()))
}

/// The approval form's target.
///
/// Approving mints the authorization code and hands it to the client; anything
/// else, the Deny button included, refuses and burns both single-use
/// credentials. An unrecognized action therefore fails closed.
pub(crate) async fn post_complete_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	Form(params): Form<CompleteParams>,
) -> Response {
	// A browser is on the other end of this form, so failures render as a page
	// rather than the API error envelope.
	match handle_approval(&services, client, &params).await {
		| Ok(response) => response,
		| Err(e) => account_error_response(&e),
	}
}

async fn handle_approval(
	services: &Services,
	client: IpAddr,
	params: &CompleteParams,
) -> Result<Response> {
	services.oauth.check_rate_limit(client)?;

	if approved(params.action.as_deref()) {
		accept_code(services, params).await
	} else {
		refuse_code(services, params).await
	}
}

/// Whether the submitted form approved the sign-in.
///
/// Only the exact `approve` action does. The Deny button, an unknown action and
/// a submission carrying no action at all are all refusals, so a form this
/// server did not write cannot talk its way into a code.
fn approved(action: Option<&str>) -> bool { action == Some("approve") }

impl Approval<'_> {
	/// Render the approval prompt.
	///
	/// Every field is HTML-escaped on the way in, the client-supplied ones
	/// included, since a client names itself at registration and nothing there
	/// constrains the text. One `format!` substitutes them all, so no value can
	/// land in another's slot.
	fn render(&self) -> String {
		let user = html_escape(self.user_id.as_str());
		let client = html_escape(self.client_name);
		let website = html_escape(self.client_uri);
		let redirect = html_escape(self.redirect_uri);
		let scope = html_escape(self.scope);
		let req_id = html_escape(self.req_id);
		let token = html_escape(self.login_token);

		format!(
			r#"<!DOCTYPE html>
		<html lang="en">
			<head>
				{ACCOUNT_HEAD}
				<title>Authorize application</title>
			</head>
			<body>
				<h1>Authorize application</h1>
				<p>An application is asking to sign in as <strong>{user}</strong>.</p>
				<p>Application: <strong>{client}</strong></p>
				<p>Website: <code>{website}</code></p>
				<p>Sign-in is handed back to: <code>{redirect}</code></p>
				<p>Requested access: <code>{scope}</code></p>
				<p class="warn">
					Approve only if you started this sign-in yourself. Approving gives
					this application access to your account.
				</p>
				<form method="POST" action="/_tuwunel/oidc/_complete">
					<input type="hidden" name="oidc_req_id" value="{req_id}">
					<input type="hidden" name="loginToken" value="{token}">
					<button type="submit" name="action" value="approve" class="primary">
						Approve
					</button>
					<button type="submit" name="action" value="deny" class="danger">
						Deny
					</button>
				</form>
			</body>
		</html>"#
		)
	}
}

/// Whether the authorization code goes out without asking the user first.
///
/// Two conditions waive the prompt: the operator turned it off with
/// `oidc_require_client_approval`, or listed this redirect target in the
/// registration allowlist. Neither holds on a server running open dynamic
/// registration, where an attacker can register a client of their own and phish
/// an authorization link. An initial access token deliberately does not waive
/// it, since closing registration says nothing about the clients that were
/// already registered when it closed.
fn approval_waived(services: &Services, redirect_uri: &str) -> bool {
	let config = &services.config;

	!config.oidc_require_client_approval
		|| redirect_allowlisted(&config.oidc_registration_allowed_redirect_hosts, redirect_uri)
}

/// Mint the authorization code for an approved sign-in.
///
/// The pending request is read here rather than carried across from the page,
/// because the approval arrives as its own HTTP request. Retiring it is
/// `release_code`'s job.
async fn accept_code(services: &Services, params: &CompleteParams) -> Result<Response> {
	let auth_req = services
		.oauth
		.get_server()?
		.peek_auth_request(&params.oidc_req_id)
		.await?;

	release_code(services, &auth_req, params).await
}

/// Retire the pending authorization request, mint the code, and hand it to the
/// client's redirect target.
///
/// The request is retired before the code exists, so a resubmitted form finds
/// nothing to mint against. The login token is spent on the same pass, which
/// makes this the one-shot tail of both entry points.
async fn release_code(
	services: &Services,
	auth_req: &AuthRequest,
	params: &CompleteParams,
) -> Result<Response> {
	let oidc = services.oauth.get_server()?;

	oidc.remove_auth_request(&params.oidc_req_id);

	let user_id = consume_login_token(services, Some(&params.login_token)).await?;
	let code = oidc.create_auth_code(auth_req, user_id);
	let redirect_url = Url::parse(&auth_req.redirect_uri)
		.map_err(|_| err!(Request(InvalidParam("Invalid redirect_uri"))))
		.map(|mut url| {
			let pairs = once(("code", code.as_str()))
				.chain(auth_req.state.as_deref().map(|s| ("state", s)));

			match auth_req.response_mode.as_deref() {
				| Some("fragment") => {
					let body = form_urlencoded::Serializer::new(String::new())
						.extend_pairs(pairs)
						.finish();

					url.set_fragment(Some(&body));
				},
				| _ => {
					url.query_pairs_mut().extend_pairs(pairs);
				},
			}

			url
		})?;

	let native = redirect_url.scheme() == "https"
		&& oidc
			.get_client(&auth_req.client_id)
			.await
			.is_ok_and(|client| client.application_type.as_deref() == Some("native"));

	Ok(if needs_interstitial(&redirect_url, native) {
		account_html_response(StatusCode::OK, complete_continue_html(redirect_url.as_str()))
	} else {
		Redirect::temporary(redirect_url.as_str()).into_response()
	})
}

/// Discard a refused authorization.
///
/// Both single-use credentials are burned, so a refusal cannot be resumed by
/// replaying the form. The request is removed without being read, since nothing
/// here needs its contents.
async fn refuse_code(services: &Services, params: &CompleteParams) -> Result<Response> {
	services
		.oauth
		.get_server()?
		.remove_auth_request(&params.oidc_req_id);

	consume_login_token(services, Some(&params.login_token))
		.await
		.ok();

	Ok(account_html_response(StatusCode::OK, DENIED_HTML.to_owned()))
}

/// Whether the auth code is handed back via a "Continue" interstitial (a user
/// gesture) rather than a direct redirect. True for private-use reverse-DNS app
/// schemes (RFC 8252, e.g. `io.element.android`), which Chrome will not
/// auto-follow, and for a native client's `https` universal link, which iOS
/// opens into the app only on a user navigation, not a silent 3xx. Web `https`
/// and native `http` loopback redirect directly; a `javascript:` or `data:`
/// target is neither dotted nor `https`, so it stays an inert `Location`, never
/// a clickable link.
fn needs_interstitial(redirect_url: &Url, native: bool) -> bool {
	redirect_url.scheme().contains('.') || (native && redirect_url.scheme() == "https")
}

fn complete_continue_html(redirect_url: &str) -> String {
	let href = html_escape(redirect_url);

	format!(
		r#"<!DOCTYPE html>
		<html lang="en">
			<head>
				{ACCOUNT_HEAD}
				<title>Continue</title>
			</head>
			<body>
				<h1>Almost there</h1>
				<p>Continue to return to your app and finish signing in.</p>
				<div class="nav">
					<a href="{href}">Continue</a>
				</div>
			</body>
		</html>"#
	)
}

#[cfg(test)]
mod tests {
	use ruma::user_id;
	use url::Url;

	use super::{Approval, approved, complete_continue_html, needs_interstitial};

	fn approval(client_name: &str, login_token: &str) -> String {
		Approval {
			user_id: user_id!("@alice:example.com"),
			client_name,
			client_uri: "https://attacker.example/app",
			redirect_uri: "https://attacker.example/callback",
			scope: "urn:matrix:org.matrix.msc2967.client:api:*",
			req_id: "reqid",
			login_token,
		}
		.render()
	}

	#[test]
	fn only_the_approve_action_approves() {
		assert!(approved(Some("approve")));

		assert!(!approved(Some("deny")));
		assert!(!approved(Some("Approve")));
		assert!(!approved(Some("approve ")));
		assert!(!approved(Some("")));
		assert!(!approved(None));
	}

	#[test]
	fn interstitial_for_native_or_reverse_dns() {
		let needs = |u: &str, native: bool| needs_interstitial(&Url::parse(u).unwrap(), native);

		// Reverse-DNS app scheme (Android): interstitial regardless of client type.
		assert!(needs("io.element.android:/?code=a&state=b", true));
		assert!(needs("io.element.android:/?code=a&state=b", false));
		// Native https universal link (Element X iOS): now interstitial.
		assert!(needs("https://element.io/oauth/ios/io.element.elementx?code=a", true));
		// Web https client (Element Web): direct redirect, no friction.
		assert!(!needs("https://app.example.com/cb?code=a", false));
		// Native http loopback (desktop local server): direct redirect.
		assert!(!needs("http://127.0.0.1/cb?code=a", true));
		// Dangerous bare schemes never become a clickable link, even when native.
		assert!(!needs("javascript:alert(1)", true));
		assert!(!needs("data:text/html,x", true));
	}

	#[test]
	fn continue_html_links_escaped_redirect() {
		let html = complete_continue_html("io.element.android:/?code=a&state=b");

		assert!(html.contains(r#"href="io.element.android:"#));
		assert!(html.contains("&amp;"));
		assert!(html.contains("Continue"));
	}

	#[test]
	fn approval_names_the_client_and_carries_the_token() {
		let html = approval("Test client", "tok");

		assert!(html.contains("@alice:example.com"));
		assert!(html.contains("Test client"));
		assert!(html.contains("https://attacker.example/callback"));
		assert!(html.contains(r#"name="loginToken" value="tok""#));
		assert!(html.contains(r#"name="oidc_req_id" value="reqid""#));
		assert!(html.contains(r#"value="approve""#));
		assert!(html.contains(r#"value="deny""#));
	}

	#[test]
	fn approval_escapes_client_metadata() {
		let html = approval("<script>alert(1)</script>", "tok");

		assert!(!html.contains("<script>"));
		assert!(html.contains("&lt;script&gt;"));
	}

	#[test]
	fn client_metadata_cannot_reach_the_token_slot() {
		// A client naming itself after a template placeholder stays literal text.
		let html = approval("{token}{req_id}{user}", "supersecret");

		assert_eq!(html.matches("supersecret").count(), 1);
		assert!(html.contains("{token}{req_id}{user}"));
	}
}
