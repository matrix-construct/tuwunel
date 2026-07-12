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

	Ok(if needs_interstitial(&redirect_url) {
		account_html_response(StatusCode::OK, complete_continue_html(redirect_url.as_str()))
	} else {
		Redirect::temporary(redirect_url.as_str()).into_response()
	})
}

/// Whether the auth code is handed back via a "Continue" interstitial (a user
/// gesture) rather than a direct redirect. True only for private-use
/// reverse-DNS app schemes (RFC 8252, e.g. `io.element.android`), which Chrome
/// will not auto-follow. http(s) and any bare scheme redirect directly, so a
/// dangerous `javascript:`/`data:` target stays an inert `Location`, never a
/// clickable link.
fn needs_interstitial(redirect_url: &Url) -> bool { redirect_url.scheme().contains('.') }

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
	fn interstitial_only_for_reverse_dns_schemes() {
		let needs = |u: &str| needs_interstitial(&Url::parse(u).unwrap());

		// Private-use reverse-DNS app scheme (RFC 8252): interstitial.
		assert!(needs("io.element.android:/?code=a&state=b"));
		// Web and loopback: direct redirect, unchanged.
		assert!(!needs("https://app.example.com/cb?code=a"));
		assert!(!needs("http://127.0.0.1/cb?code=a"));
		// Dangerous bare schemes must not become a clickable link.
		assert!(!needs("javascript:alert(1)"));
		assert!(!needs("data:text/html,x"));
	}

	#[test]
	fn continue_html_links_escaped_redirect() {
		let html = complete_continue_html("io.element.android:/?code=a&state=b");

		assert!(html.contains(r#"href="io.element.android:"#));
		assert!(html.contains("&amp;"));
		assert!(html.contains("Continue"));
	}
}
