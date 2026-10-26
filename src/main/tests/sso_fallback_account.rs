#![cfg(test)]

mod admin;

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;
mod fixture;

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
	Client, Response, StatusCode, Url,
	header::{AUTHORIZATION, COOKIE, LOCATION, SET_COOKIE},
	redirect::Policy,
};
use serde_json::{Value, json};
use tuwunel_core::{Err, Result, err, result::NotFound, ruma::OwnedUserId};
use tuwunel_service::{
	Services,
	oauth::{Deletions, Session, UserInfo, fallback_localpart, unique_id_sub},
};

use self::{
	admin::{accepted, refused},
	fixture::boot,
};

type TokenForm = HashMap<String, String>;

const IDP: &str = "test-idp";
const CLIENT: &str = "https://client.example/callback";

/// A taken username never hands an identity another identity's fallback account.
///
/// Alice claims the localpart derived from Bob's subject as her username, so
/// Bob, whose own claimed username is then taken, falls back to her account.
/// His callback must answer 400 and leave his identity unlinked, while Carol
/// still reaches her own fallback account after an admin unlinks her. Before
/// that, while her identity binding cannot be read, a user-wide delete keeps
/// her session and a forced association is refused. The unlink also removes
/// her session on a provider no longer configured.
#[test]
fn sso_fallback_account() -> Result {
	let provider = TcpListener::bind(("127.0.0.1", 0))?;
	let issuer = format!("http://{}", provider.local_addr()?);

	let option = |key: &str, value: &str| format!("identity_provider.{IDP}.{key}=\"{value}\"");
	let options = [
		"authentication.oidc.registration_allowed_redirect_hosts=[\"client.example\"]",
		&option("client_id", IDP),
		&option("client_secret", "test-secret"),
		&option("brand", "test"),
		&option("issuer_url", &issuer),
	];

	provider.set_nonblocking(true)?;

	boot("sso-fallback-account", options, async move |services, base| {
		let serve = serve_provider(provider, issuer);

		select(pin!(serve), pin!(exercise(services, base)))
			.await
			.factor_first()
			.0
	})
}

/// Serve the identity provider endpoints until the test is done.
///
/// A callback's code is `sub:username`, which the token endpoint returns as the
/// access token and userinfo splits into `sub` and `preferred_username`, so each
/// callback names the identity it signs in as. Serving stops only on failure.
async fn serve_provider(listener: TcpListener, issuer: String) -> Result {
	let app = Router::new()
		.route("/.well-known/openid-configuration", get(discover))
		.route("/token", post(token))
		.route("/userinfo", get(userinfo))
		.with_state(issuer);

	from_tcp(listener)?
		.serve(app.into_make_service())
		.await?;

	Err!("the identity provider stand-in stopped serving")
}

async fn discover(State(issuer): State<String>) -> Json<Value> {
	Json(json!({
		"issuer": issuer,
		"authorization_endpoint": format!("{issuer}/authorize"),
		"token_endpoint": format!("{issuer}/token"),
		"userinfo_endpoint": format!("{issuer}/userinfo"),
	}))
}

async fn token(Form(form): Form<TokenForm>) -> Json<Value> {
	Json(json!({
		"access_token": form["code"],
		"token_type": "Bearer",
	}))
}

async fn userinfo(headers: HeaderMap) -> Json<Value> {
	let (sub, username) = headers[AUTHORIZATION]
		.to_str()
		.expect("bearer token")
		.trim_start_matches("Bearer ")
		.split_once(':')
		.expect("subject and username");

	Json(json!({
		"sub": sub,
		"preferred_username": username,
	}))
}

async fn exercise(services: &Services, base: &str) -> Result {
	let client = Client::builder()
		.redirect(Policy::none())
		.timeout(Duration::from_secs(10))
		.build()?;

	let provider = services.oauth.providers.get(IDP).await?;
	let bob = unique_id_sub((&provider, "bob"))?;
	let fallback = fallback_localpart(&bob);

	let response = sign_in(&client, base, "alice", &fallback).await?;

	assert_eq!(fallback, signed_in(services, &response).await?.localpart());

	let response = sign_in(&client, base, "bob", &fallback).await?;

	assert_eq!(response.status(), StatusCode::BAD_REQUEST);

	let error: Value = response.json().await?;
	let linked = services
		.oauth
		.sessions
		.get_by_unique_id(&bob)
		.await;

	assert_eq!(error["errcode"], "M_USER_IN_USE");
	assert!(linked.is_not_found(), "bob's identity is linked to no account");

	let carol = unique_id_sub((&provider, "carol"))?;
	let response = sign_in(&client, base, "carol", &fallback).await?;
	let account = signed_in(services, &response).await?;

	assert_eq!(fallback_localpart(&carol), account.localpart());

	let bindings = &services.db["oauthuniqid_oauthid"];
	let sess_id = services
		.oauth
		.sessions
		.get_sess_id_by_unique_id(&carol)
		.await?;

	// A lone continuation byte is not UTF-8, so the read fails without reading as absent.
	bindings.insert(&carol, [0x80]);

	let Deletions { complete, unresolved, absent, unreadable } = services
		.oauth
		.delete_user_sessions(&account)
		.await?;

	let associate = format!("query oauth associate {IDP} {account} --claim sub=carol --force");
	let refusal = refused(services, &associate, "could not be read and were kept").await;
	let pending = services
		.oauth
		.sessions
		.is_user_association_pending(&account);

	let kept = services.oauth.sessions.get(&sess_id).await;

	bindings.insert(&carol, &sess_id);

	assert_eq!((complete, unresolved, absent, unreadable), (0, 0, 0, 1));
	assert!(kept.is_ok(), "carol's session is kept while her binding cannot be read");
	assert!(!pending, "the refused association adds no pending claim");
	refusal?;

	let user_info = UserInfo {
		sub: "carol".to_owned(),
		..Default::default()
	};

	let stale = Session {
		idp_id: Some("removed-idp".to_owned()),
		sess_id: Some("stale".to_owned()),
		user_id: Some(account.clone()),
		user_info: Some(user_info),
		..Default::default()
	};

	services.oauth.sessions.put(&stale).await;

	// Unlinked by an admin, carol's own fallback account is hers again.
	accepted(services, &format!("query oauth delete {account} --force")).await?;

	let linked = services
		.oauth
		.sessions
		.get_by_unique_id(&carol)
		.await;

	let stale = services.oauth.sessions.get("stale").await;

	assert!(linked.is_not_found(), "carol's identity is linked to no account");
	assert!(stale.is_missing(), "carol's session on a removed provider is deleted");

	let response = sign_in(&client, base, "carol", &fallback).await?;

	assert_eq!(signed_in(services, &response).await?, account);

	Ok(())
}

/// Sign in at the provider as `sub` claiming `username`.
///
/// The callback's response returns unfollowed, so the caller reads its status
/// and where it redirects.
async fn sign_in(client: &Client, base: &str, sub: &str, username: &str) -> Result<Response> {
	let url = format!("{base}/_matrix/client/v3/login/sso/redirect/{IDP}");
	let redirect = client
		.get(url)
		.query(&[("redirectUrl", CLIENT)])
		.send()
		.await?;

	assert_eq!(redirect.status(), StatusCode::FOUND);

	let state = parameter(&location(&redirect)?, "state");
	let cookie = redirect.headers()[SET_COOKIE]
		.to_str()
		.expect("cookie header text")
		.split(';')
		.next()
		.expect("cookie pair");

	let url = format!("{base}/_matrix/client/unstable/login/sso/callback/{IDP}");
	let code = format!("{sub}:{username}");

	client
		.get(url)
		.query(&[("code", code.as_str()), ("state", &state)])
		.header(COOKIE, cookie)
		.send()
		.await
		.map_err(Into::into)
}

/// Resolve the account named by a finished sign-in's login token.
///
/// Fails when the callback redirected anywhere but the client.
async fn signed_in(services: &Services, response: &Response) -> Result<OwnedUserId> {
	let destination = location(response)?;

	if !destination.as_str().starts_with(CLIENT) {
		return Err!("sign-in finished at {destination} rather than the client");
	}

	services
		.users
		.find_from_login_token(&parameter(&destination, "loginToken"))
		.await
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
