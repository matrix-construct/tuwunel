use std::iter::once;

use axum::{
	extract::State,
	response::{IntoResponse, Redirect, Response},
};
use http::StatusCode;
use serde::Deserialize;
use tuwunel_core::{Result, err, utils::html::escape as html_escape};
use url::{Url, form_urlencoded};

use super::account::{ACCOUNT_HEAD, account_html_response};

#[derive(Debug, Deserialize)]
pub(crate) struct CompleteParams {
	oidc_req_id: String,
	#[serde(rename = "loginToken")]
	login_token: String,
}

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
		.take_auth_request(&params.oidc_req_id)
		.await?;

	let user_id = services
		.users
		.find_from_login_token(&params.login_token)
		.await
		.map_err(|_| err!(Request(Forbidden("Invalid or expired login token"))))?;

	let code = oidc.create_auth_code(&auth_req, user_id);
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
	use url::Url;

	use super::{complete_continue_html, needs_interstitial};

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
}
