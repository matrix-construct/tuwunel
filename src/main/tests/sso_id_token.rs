#![cfg(test)]

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

mod fixture;

#[expect(
	dead_code,
	reason = "Minting by user serves the JWT login tests."
)]
mod jwt;

mod sso;

use std::{collections::HashMap, net::TcpListener, pin::pin, time::Duration};

use axum::{
	Form, Json, Router,
	extract::State,
	http::HeaderMap,
	routing::{get, post},
};
use axum_server::from_tcp;
use futures::future::select;
use reqwest::{
	Client, Response, StatusCode,
	header::{AUTHORIZATION, LOCATION},
	redirect::Policy,
};
use serde_json::{Value, json};
use tuwunel_core::{Err, Result, utils::time::now_secs};
use tuwunel_service::Services;

use self::{
	fixture::boot,
	jwt::sign,
	sso::{CLIENT, authorize, complete, exists, flag, provider, signed_in},
};

type TokenForm = HashMap<String, String>;

/// The server under test and the browser signing in to it.
#[derive(Clone, Copy)]
struct Site<'a> {
	services: &'a Services,
	base: &'a str,
	client: &'a Client,
}

const CHECKED: &str = "checked-idp";
const UNCHECKED: &str = "unchecked-idp";
const APPLE: &str = "apple-idp";
const MANUAL: &str = "manual-idp";
const CALLBACK: &str = "_tuwunel/sso/callback";

/// A provider's ID token has to answer the sign-in that asked for it.
///
/// A token naming this provider, this client, a future expiry and the sign-in's
/// own nonce signs in, as does a response with no ID token at all. One that is
/// no JWT, or misses on any of those four claims, is refused and leaves no
/// account, unless the provider has `check_id_token` turned off. The Apple
/// brand, whose identity comes from the token itself, is held to the same
/// checks. A provider with discovery turned off signs in on its default
/// endpoints, with no discovery document to fetch.
#[test]
fn sso_id_token() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let issuer = format!("http://{}", listener.local_addr()?);
	let options: Vec<_> = [
		provider(CHECKED, "checked", &issuer),
		provider(UNCHECKED, "unchecked", &issuer),
		provider(APPLE, "appleoidc", &format!("{issuer}/apple")),
		provider(MANUAL, "manual", &format!("{issuer}/manual")),
	]
	.into_iter()
	.flatten()
	.chain([
		flag(UNCHECKED, "check_id_token", false),
		flag(MANUAL, "discovery", false),
		"logging.enable=false".to_owned(),
		"authentication.oidc.registration_allowed_redirect_hosts=[\"client.example\"]".to_owned(),
	])
	.collect();

	let options = options.iter().map(String::as_str);

	listener.set_nonblocking(true)?;

	boot("sso-id-token", options, async move |services, base| {
		let client = Client::builder()
			.redirect(Policy::none())
			.timeout(Duration::from_secs(10))
			.build()?;

		let serve = serve_provider(listener, issuer);
		let exercise = exercise(Site { services, base, client: &client });

		select(pin!(serve), pin!(exercise))
			.await
			.factor_first()
			.0
	})
}

/// Serve the identity provider endpoints until the test is done.
///
/// A callback's code is `mode.sub.nonce`. The token endpoint returns `sub` as
/// the access token, which userinfo reports back, and an ID token for `sub`
/// answering that nonce, in the shape `mode` names. The Apple mount has no
/// userinfo endpoint and the manual mount no discovery document. Serving stops
/// only on failure.
async fn serve_provider(listener: TcpListener, issuer: String) -> Result {
	let apple = Router::new()
		.route("/.well-known/openid-configuration", get(discover))
		.route("/token", post(token))
		.with_state(format!("{issuer}/apple"));

	let manual = Router::new()
		.route("/token", post(token))
		.route("/userinfo", get(userinfo))
		.with_state(format!("{issuer}/manual"));

	let app = Router::new()
		.route("/.well-known/openid-configuration", get(discover))
		.route("/token", post(token))
		.route("/userinfo", get(userinfo))
		.with_state(issuer)
		.nest("/apple", apple)
		.nest("/manual", manual);

	from_tcp(listener)?
		.serve(app.into_make_service())
		.await?;

	Err!("the identity provider stand-in stopped serving")
}

async fn discover(State(issuer): State<String>) -> Json<Value> {
	Json(json!({ "issuer": issuer }))
}

async fn token(State(issuer): State<String>, Form(form): Form<TokenForm>) -> Json<Value> {
	let (mode, identity) = form["code"]
		.split_once('.')
		.expect("mode and identity");

	let (sub, nonce) = identity
		.split_once('.')
		.expect("subject and nonce");

	let now = now_secs();
	let claims = json!({
		"iss": issuer,
		"sub": sub,
		"email": format!("{sub}@example.org"),
		"aud": form["client_id"],
		"azp": form["client_id"],
		"exp": now.saturating_add(300),
		"iat": now,
		"nonce": nonce,
	});

	let id_token = match mode {
		| "absent" => Value::Null,
		| "opaque" => format!("id.{sub}").into(),
		| _ => sign("stand-in", &spoiled(claims, mode))
			.expect("signed ID token")
			.into(),
	};

	json!({
		"access_token": sub,
		"id_token": id_token,
		"token_type": "Bearer",
	})
	.into()
}

/// The claims with the one `mode` names replaced by a value no check accepts,
/// or unchanged when `mode` names no claim.
fn spoiled(mut claims: Value, mode: &str) -> Value {
	let wrong = match mode {
		| "iss" => json!("https://elsewhere.example"),
		| "aud" => json!("another-client"),
		| "exp" => json!(1),
		| "nonce" => json!("another-nonce"),
		| _ => return claims,
	};

	claims[mode] = wrong;
	claims
}

async fn userinfo(headers: HeaderMap) -> Json<Value> {
	let sub = headers[AUTHORIZATION]
		.to_str()
		.expect("bearer token")
		.trim_start_matches("Bearer ");

	Json(json!({ "sub": sub, "preferred_username": sub }))
}

async fn exercise(site: Site<'_>) -> Result {
	accepted(site, CHECKED, "valid", "conforming").await?;
	accepted(site, CHECKED, "absent", "tokenless").await?;
	accepted(site, UNCHECKED, "opaque", "unchecked").await?;
	accepted(site, APPLE, "valid", "apple").await?;
	accepted(site, MANUAL, "valid", "manual").await?;
	refused(site, CHECKED, "opaque").await?;
	refused(site, CHECKED, "iss").await?;
	refused(site, CHECKED, "aud").await?;
	refused(site, CHECKED, "exp").await?;
	refused(site, CHECKED, "nonce").await?;
	refused(site, APPLE, "aud").await
}

/// A sign-in the provider answers in `mode` lands in the account named `sub`.
async fn accepted(site: Site<'_>, idp: &str, mode: &str, sub: &str) -> Result {
	let response = sign_in(site, idp, mode, sub).await?;
	let user_id = signed_in(site.services, &response).await?;

	assert_eq!(user_id.localpart(), sub, "{idp} answering {mode}");

	Ok(())
}

/// Sign in at provider `idp` as `sub`, the provider answering in `mode`.
async fn sign_in(site: Site<'_>, idp: &str, mode: &str, sub: &str) -> Result<Response> {
	let Site { base, client, .. } = site;
	let grant = authorize(client, base, idp, CLIENT).await?;
	let callback = format!("{base}/{CALLBACK}/{idp}");
	let code = format!("{mode}.{sub}.{}", grant.nonce);

	complete(client, &callback, &grant, &code).await
}

/// A sign-in the provider answers in `mode` is refused, with no login token and
/// no account.
async fn refused(site: Site<'_>, idp: &str, mode: &str) -> Result {
	let sub = format!("{mode}-{idp}");
	let response = sign_in(site, idp, mode, &sub).await?;

	assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{idp} answering {mode}");
	assert!(!response.headers().contains_key(LOCATION), "{idp} answering {mode} redirected");
	assert!(!exists(site.services, &sub).await?, "{idp} answering {mode} left an account");

	Ok(())
}
