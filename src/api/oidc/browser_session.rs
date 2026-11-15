//! The cookie that keeps a browser signed in to the built-in login page.
//!
//! A password sign-in sets it, a device approval honors it in place of the
//! password, and the consent page's account switch removes it.

use axum::response::AppendHeaders;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use http::header::{HeaderName, SET_COOKIE};
use ruma::UserId;
use tuwunel_core::debug_warn;
use tuwunel_service::{Services, users::BrowserSession};

use super::require_account_usable;

type SetCookie = AppendHeaders<[(HeaderName, String); 1]>;

const COOKIE_NAME: &str = "tuwunel_oidc_session";

/// The path every page that reads or clears the cookie lives under.
///
/// A removal must name the same path to replace the cookie (RFC 6265 section
/// 5.3).
const COOKIE_PATH: &str = "/_tuwunel/oidc";

pub(super) fn session_token(cookies: &CookieJar) -> Option<&str> {
	cookies.get(COOKIE_NAME).map(Cookie::value)
}

/// The user a browser is signed in as, while the session may stand in for a
/// password.
///
/// It counts only while the login page and password login are both on and the
/// account is usable; otherwise the request is handled as if it carried no
/// cookie at all.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn session_user<'a>(
	services: &Services,
	token: Option<&'a str>,
) -> Option<&'a UserId> {
	let authentication = &services.config.authentication;
	let enabled = authentication.oidc.native_auth && authentication.login.with_password;
	let user_id = services
		.users
		.find_browser_session(token.filter(|_| enabled)?)
		.await?;

	require_account_usable(services, user_id)
		.await
		.inspect_err(|error| debug_warn!(%user_id, %error, "Browser session not honored"))
		.is_ok()
		.then_some(user_id)
}

pub(super) fn session_cookie(session: BrowserSession) -> SetCookie {
	let max_age = session
		.lifetime
		.try_into()
		.expect("a session lifetime of at most 90 days fits a cookie Max-Age");

	let cookie = Cookie::build((COOKIE_NAME, session.token))
		.path(COOKIE_PATH)
		.max_age(max_age)
		.same_site(SameSite::Lax)
		.secure(true)
		.http_only(true)
		.build();

	set_cookie(&cookie)
}

pub(super) fn removal_cookie() -> SetCookie {
	let cookie = Cookie::build((COOKIE_NAME, ""))
		.path(COOKIE_PATH)
		.removal()
		.build();

	set_cookie(&cookie)
}

/// One `Set-Cookie` header carrying `cookie`.
///
/// It is appended to the response, so any other cookie the response sets
/// survives.
fn set_cookie(cookie: &Cookie<'_>) -> SetCookie {
	// CookieJar percent-decodes the value on read; encode it so the token survives.
	AppendHeaders([(SET_COOKIE, cookie.encoded().to_string())])
}
