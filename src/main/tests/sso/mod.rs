//! The browser's half of a provider sign-in, for tests that stand in for the
//! identity provider.
//!
//! A test configures its providers with [`provider`] and [`flag`], calls
//! [`authorize`], has its stand-in issue a code that answers the grant's nonce,
//! returns to the server with [`complete`], and reads the outcome with
//! [`signed_in`] or from the response itself.

use reqwest::{
	Client, Response, StatusCode, Url,
	header::{COOKIE, LOCATION, SET_COOKIE},
};
use tuwunel_core::{
	Err, Result, err,
	ruma::{OwnedUserId, UserId},
};
use tuwunel_service::Services;

/// Where a finished sign-in returns its login token.
pub(crate) const CLIENT: &str = "https://client.example/callback";

/// What the authorization redirect hands the browser to carry to the callback.
pub(crate) struct Grant {
	/// The nonce the provider's ID token has to answer.
	pub(crate) nonce: String,
	state: String,
	cookie: String,
}

/// The options configuring provider `idp` under `brand` at `issuer`.
pub(crate) fn provider(idp: &str, brand: &str, issuer: &str) -> [String; 4] {
	let option = |key: &str, value: &str| format!("identity_provider.{idp}.{key}=\"{value}\"");

	[
		option("client_id", idp),
		option("client_secret", "test-secret"),
		option("brand", brand),
		option("issuer_url", issuer),
	]
}

/// The option turning provider `idp`'s `key` on or off.
pub(crate) fn flag(idp: &str, key: &str, on: bool) -> String {
	format!("identity_provider.{idp}.{key}={on}")
}

/// Start a sign-in at provider `idp`, bound for `destination`.
///
/// # Panics
///
/// Panics unless the server answers with a redirect that sets the grant cookie.
pub(crate) async fn authorize(
	client: &Client,
	base: &str,
	idp: &str,
	destination: &str,
) -> Result<Grant> {
	let redirect = client
		.get(format!("{base}/_matrix/client/v3/login/sso/redirect/{idp}"))
		.query(&[("redirectUrl", destination)])
		.send()
		.await?;

	assert_eq!(redirect.status(), StatusCode::FOUND);

	let authorization = location(&redirect)?;
	let cookie = redirect.headers()[SET_COOKIE]
		.to_str()
		.expect("cookie header text")
		.split(';')
		.next()
		.expect("cookie pair")
		.to_owned();

	Ok(Grant {
		nonce: parameter(&authorization, "nonce"),
		state: parameter(&authorization, "state"),
		cookie,
	})
}

/// Return to the server's callback URL with the provider's code.
///
/// With a client that does not follow redirects, the caller reads the
/// response's status and where it redirects.
pub(crate) async fn complete(
	client: &Client,
	callback: &str,
	grant: &Grant,
	code: &str,
) -> Result<Response> {
	client
		.get(callback)
		.query(&[("code", code), ("state", &grant.state)])
		.header(COOKIE, &grant.cookie)
		.send()
		.await
		.map_err(Into::into)
}

/// Resolve the account named by a finished sign-in's login token.
///
/// Fails when the callback redirected anywhere but the client.
pub(crate) async fn signed_in(services: &Services, response: &Response) -> Result<OwnedUserId> {
	let destination = location(response)?;

	if !destination.as_str().starts_with(CLIENT) {
		return Err!("sign-in finished at {destination} rather than the client");
	}

	services
		.users
		.find_from_login_token(&parameter(&destination, "loginToken"))
		.await
}

/// Whether the server has an account under `localpart`.
pub(crate) async fn exists(services: &Services, localpart: &str) -> Result<bool> {
	let user_id = UserId::parse_with_server_name(localpart, services.globals.server_name())?;

	Ok(services.users.exists(&user_id).await)
}

fn location(response: &Response) -> Result<Url> {
	response
		.headers()
		.get(LOCATION)
		.ok_or_else(|| err!("{} response has no location", response.status()))?
		.to_str()
		.expect("location header text")
		.parse()
		.map_err(Into::into)
}

fn parameter(url: &Url, name: &str) -> String {
	url.query_pairs()
		.find(|(key, _)| key == name)
		.expect("query parameter")
		.1
		.into_owned()
}
