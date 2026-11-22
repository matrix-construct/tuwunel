#![cfg(test)]

mod admin;
mod appservice;

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;
mod fixture;

use std::{collections::HashMap, net::TcpListener, pin::pin, sync::Mutex, time::Duration};

use axum::{
	Form, Json, Router,
	extract::State,
	http::HeaderMap,
	routing::{get, post},
};
use axum_server::from_tcp;
use futures::{TryFutureExt, future::select};
use reqwest::{
	Client, Response, StatusCode, Url,
	header::{AUTHORIZATION, COOKIE, LOCATION, SET_COOKIE},
	redirect::Policy,
};
use serde_json::{Value, json};
use tracing::{
	Level,
	level_filters::{LevelFilter, STATIC_MAX_LEVEL},
	subscriber::set_global_default,
};
use tracing_subscriber::fmt::{MakeWriter, fmt, format::FmtSpan};
use tuwunel_core::{
	Err, Result, err,
	result::NotFound,
	ruma::{
		OwnedUserId, UserId,
		api::{
			appservice::{Namespace, Namespaces, Registration, RegistrationInit},
			client::uiaa::{AuthType, UiaaInfo},
			error::ErrorKind,
		},
	},
};
use tuwunel_service::{
	Services,
	oauth::{Deletions, Session, UserInfo, fallback_localpart, unique_id_sub},
};

use self::{
	admin::{accepted, refused},
	appservice::{Bridge, register_appservice},
	fixture::boot,
};

type TokenForm = HashMap<String, String>;

#[derive(Clone, Copy)]
struct SignIn<'a> {
	idp: &'a str,
	callback: &'a str,
	destination: &'a str,
	sub: &'a str,
	username: &'a str,
	mailbox: &'a str,
}

const IDP: &str = "test-idp";
const TRUSTED: &str = "trusted-idp";
const STRICT: &str = "strict-idp";
const CLIENT: &str = "https://client.example/callback";
const LEGACY_CALLBACK: &str = "_matrix/client/unstable/login/sso/callback";
const CALLBACK: &str = "_tuwunel/sso/callback";

static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// A taken username never hands an identity another identity's fallback account.
///
/// Alice claims the localpart derived from Bob's subject as her username, so
/// Bob, whose own claimed username is then taken, falls back to her account.
/// His callback must answer 400 and leave his identity unlinked, while Carol
/// still reaches her own fallback account after an admin unlinks her, which
/// also removes her session on a provider no longer configured. Before that,
/// while her identity binding cannot be read, a user-wide delete keeps her
/// session and a forced association is refused. Alice's provider-verified
/// address is bound to her new account, and Carol's registration leaves the
/// identical address with Alice. The provider sets no `callback_url` and the
/// server no `well_known.client`, so the legacy callback stays mounted beside
/// the new one, and Carol's last sign-in returns through the new one. Then a
/// UIAA session's SSO stage is completed only by its own account's sign-in, and
/// last, sign-in skips the usernames an appservice reserves, local policy
/// forbids, strict syntax rejects or another server owns.
#[test]
fn sso_fallback_account() -> Result {
	let provider = TcpListener::bind(("127.0.0.1", 0))?;
	let issuer = format!("http://{}", provider.local_addr()?);

	let option =
		|idp: &str, key: &str, value: &str| format!("identity_provider.{idp}.{key}=\"{value}\"");

	let options = [
		// The capture below is the global subscriber, in the server's place.
		"logging.enable=false",
		"authentication.oidc.registration_allowed_redirect_hosts=[\"client.example\"]",
		&option(IDP, "client_id", IDP),
		&option(IDP, "client_secret", "test-secret"),
		&option(IDP, "brand", "test"),
		&option(IDP, "issuer_url", &issuer),
		&option(TRUSTED, "client_id", TRUSTED),
		&option(TRUSTED, "client_secret", "test-secret"),
		&option(TRUSTED, "brand", "trusted"),
		&option(TRUSTED, "issuer_url", &issuer),
		&format!("identity_provider.{TRUSTED}.trusted=true"),
		&option(STRICT, "client_id", STRICT),
		&option(STRICT, "client_secret", "test-secret"),
		&option(STRICT, "brand", "strict"),
		&option(STRICT, "issuer_url", &issuer),
		&format!("identity_provider.{STRICT}.unique_id_fallbacks=false"),
		"registration.forbidden_usernames=[\"^forbidden_\"]",
	];

	provider.set_nonblocking(true)?;

	let capture = fmt()
		.with_max_level(Level::TRACE)
		.with_span_events(FmtSpan::NEW)
		.with_ansi(false)
		.with_writer(|| LOGS.make_writer())
		.finish();

	set_global_default(capture).expect("the log capture is the first subscriber");

	boot("sso-fallback-account", options, async move |services, base| {
		let client = Client::builder()
			.redirect(Policy::none())
			.timeout(Duration::from_secs(10))
			.build()?;

		let serve = serve_provider(provider, issuer);
		let exercise = exercise(services, base, &client)
			.and_then(|()| reauthenticate(services, base, &client))
			.and_then(|()| reserved_usernames(services, base, &client))
			.and_then(|()| skipped_usernames(services, base, &client));

		select(pin!(serve), pin!(exercise))
			.await
			.factor_first()
			.0
	})
}

/// Serve the identity provider endpoints until the test is done.
///
/// A callback's code is `sub.username.mailbox`, which the token endpoint returns
/// as the access token and userinfo splits into `sub`, `preferred_username` and
/// the mailbox of an address at the mixed-case domain `Example.ORG`, sent with
/// `email_verified` as the string `"true"`. So each callback names the identity
/// it signs in as. Serving stops only on failure.
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
	let code = &form["code"];

	Json(json!({
		"access_token": code,
		"refresh_token": format!("refresh.{code}"),
		"id_token": format!("id.{code}"),
		"token_type": "Bearer",
	}))
}

async fn userinfo(headers: HeaderMap) -> Json<Value> {
	let (sub, claims) = headers[AUTHORIZATION]
		.to_str()
		.expect("bearer token")
		.trim_start_matches("Bearer ")
		.split_once('.')
		.expect("subject and claims");

	let (username, mailbox) = claims
		.split_once('.')
		.expect("username and mailbox");

	Json(json!({
		"sub": sub,
		"preferred_username": username,
		"email": format_args!("{mailbox}@Example.ORG"),
		"email_verified": "true",
	}))
}

async fn exercise(services: &Services, base: &str, client: &Client) -> Result {
	let provider = services.oauth.providers.get(IDP).await?;
	let bob = unique_id_sub((&provider, "bob"))?;
	let fallback = fallback_localpart(&bob);

	let response = sign_in(client, base, LEGACY_CALLBACK, "alice", &fallback).await?;
	let alice_account = signed_in(services, &response).await?;
	let email = format!("{fallback}@example.org");
	let holder = services
		.threepid
		.user_id_for_email(&email)
		.await?;

	assert_eq!(fallback, alice_account.localpart());
	assert_eq!(
		holder.as_ref(),
		Some(&alice_account),
		"alice's verified address is bound to her"
	);

	let response = sign_in(client, base, LEGACY_CALLBACK, "bob", &fallback).await?;

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
	let response = sign_in(client, base, LEGACY_CALLBACK, "carol", &fallback).await?;
	let account = signed_in(services, &response).await?;
	let holder = services
		.threepid
		.user_id_for_email(&email)
		.await?;

	assert_eq!(fallback_localpart(&carol), account.localpart());
	assert_eq!(holder, Some(alice_account), "carol's identical address stays with alice");

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

	let response = sign_in(client, base, CALLBACK, "carol", &fallback).await?;

	assert_eq!(signed_in(services, &response).await?, account);

	let session = services
		.oauth
		.sessions
		.get_by_unique_id(&carol)
		.await?;

	assert_unlogged(&session);

	Ok(())
}

/// Only an account's own sign-in completes its UIAA session's SSO stage.
///
/// Dave's deactivation asks him to re-authenticate, and Erin signs in as herself
/// for his UIAA session. The server refuses her and leaves his SSO stage open;
/// Dave then completes it by signing in himself.
async fn reauthenticate(services: &Services, base: &str, client: &Client) -> Result {
	let response = sign_in(client, base, CALLBACK, "dave", "dave").await?;
	let dave = signed_in(services, &response).await?;
	let response = sign_in(client, base, CALLBACK, "erin", "erin").await?;

	signed_in(services, &response).await?;

	let token = "sso-fallback-account-reauthentication-token";

	services
		.users
		.create_device(&dave, None, (Some(token), None), None, None, None)
		.await?;

	let response = client
		.post(format!("{base}/_matrix/client/v3/account/deactivate"))
		.bearer_auth(token)
		.json(&json!({}))
		.send()
		.await?;

	assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

	let challenge: UiaaInfo = response.json().await?;
	let session = challenge.session.expect("UIAA session");
	let destination = format!("uiaa:{session}");
	let completed = async || {
		services
			.uiaa
			.get_uiaa_session_by_session_id(&session)
			.await
			.map(|(.., uiaa)| uiaa.completed)
	};

	let erin = SignIn {
		idp: IDP,
		callback: CALLBACK,
		destination: &destination,
		sub: "erin",
		username: "erin",
		mailbox: "erin",
	};

	let response = sign_in_to(client, base, erin).await?;

	assert_eq!(response.status(), StatusCode::FORBIDDEN, "erin's sign-in is refused");
	assert_eq!(completed().await.as_deref(), Some(&[][..]), "dave's SSO stage stays open");

	let owner = SignIn {
		sub: "dave",
		username: "dave",
		mailbox: "dave",
		..erin
	};

	let response = sign_in_to(client, base, owner).await?;
	let fallback = format!("/_matrix/client/v3/auth/m.login.sso/fallback/web?session={session}");

	assert_eq!(response.headers()[LOCATION], fallback.as_str());

	let completed = completed().await;

	assert_eq!(completed.as_deref(), Some(&[AuthType::Sso][..]), "dave's sign-in completes it");

	Ok(())
}

/// Sign-in skips usernames an appservice reserves, and keeps bound accounts.
///
/// Frank's account predates the appservice claiming `reserved_` names, and his
/// bound identity still signs in to it. Henry's reserved preferred username
/// falls through to his address's mailbox. Ivy's claims and fallback are all
/// reserved, so her sign-in fails and creates no account. The trusted provider
/// does not adopt Frank's account for Grace's unbound identity claiming his
/// username.
async fn reserved_usernames(services: &Services, base: &str, client: &Client) -> Result {
	let sign_in_frank = async || {
		let response = sign_in(client, base, CALLBACK, "frank", "reserved_frank").await?;

		signed_in(services, &response).await
	};

	let frank = sign_in_frank().await?;

	assert_eq!(frank.localpart(), "reserved_frank", "frank's account predates the namespace");

	let provider = services.oauth.providers.get(IDP).await?;
	let ivy_identity = unique_id_sub((&provider, "ivy"))?;
	let fallback = fallback_localpart(&ivy_identity);
	let users = format!("^@(reserved_.*|{fallback}):localhost$");
	let bridge = Bridge {
		id: "names",
		token: "test-names-token",
		sender_localpart: "names",
		users: &users,
		aliases: None,
	};

	register_appservice(services, &bridge).await?;

	let ordinary = services
		.oauth
		.candidate_user_id("ordinary")
		.await?;

	let reserved = services
		.oauth
		.candidate_user_id("reserved_example")
		.await;

	assert_eq!(ordinary.localpart(), "ordinary");
	assert_eq!(reserved.expect_err("reserved candidate").kind(), ErrorKind::Exclusive);

	let account = sign_in_frank().await?;

	assert_eq!(account, frank, "frank's identity keeps his account");

	let henry = SignIn {
		idp: IDP,
		callback: CALLBACK,
		destination: CLIENT,
		sub: "henry",
		username: "reserved_henry",
		mailbox: "henry",
	};

	let response = sign_in_to(client, base, henry).await?;
	let account = signed_in(services, &response).await?;

	assert_eq!(account.localpart(), "henry", "henry's reserved username is skipped");
	assert!(!exists(services, "reserved_henry").await?, "no reserved account is created");

	let ivy = SignIn {
		sub: "ivy",
		username: "reserved_ivy",
		mailbox: "reserved_ivy_mail",
		..henry
	};

	let response = sign_in_to(client, base, ivy).await?;

	assert_eq!(response.status(), StatusCode::BAD_REQUEST);

	let error: Value = response.json().await?;
	let linked = services
		.oauth
		.sessions
		.get_by_unique_id(&ivy_identity)
		.await;

	assert_eq!(error["errcode"], "M_USER_IN_USE");
	assert!(linked.is_not_found(), "ivy's identity is linked to no account");

	for localpart in ["reserved_ivy", "reserved_ivy_mail", &fallback] {
		assert!(!exists(services, localpart).await?, "{localpart} has no account");
	}

	let grace = SignIn {
		idp: TRUSTED,
		sub: "grace",
		username: "reserved_frank",
		mailbox: "reserved_frank",
		..henry
	};

	let response = sign_in_to(client, base, grace).await?;
	let account = signed_in(services, &response).await?;
	let trusted = services.oauth.providers.get(TRUSTED).await?;
	let grace_identity = unique_id_sub((&trusted, "grace"))?;

	assert_ne!(account, frank, "the trusted provider does not adopt frank's account");
	assert_eq!(fallback_localpart(&grace_identity), account.localpart());

	Ok(())
}

/// Sign-in skips forbidden, malformed and reserved usernames alike.
///
/// Mia's forbidden and Noah's malformed preferred usernames each fall through to
/// the address's mailbox, as do Paul's user ID on another server and Quinn's on
/// this one with a forbidden localpart. A user namespace that is not exclusive
/// reserves nothing, while an appservice's own sender stays reserved outside its
/// namespace, so the trusted provider does not adopt it for Kate. Olivia's
/// username and mailbox are reserved at a provider whose fallback is disabled,
/// so her sign-in fails creating no account.
async fn skipped_usernames(services: &Services, base: &str, client: &Client) -> Result {
	register_shared(services).await?;

	let mia = SignIn {
		idp: IDP,
		callback: CALLBACK,
		destination: CLIENT,
		sub: "mia",
		username: "forbidden_mia",
		mailbox: "mia",
	};

	let response = sign_in_to(client, base, mia).await?;
	let account = signed_in(services, &response).await?;

	assert_eq!(account.localpart(), "mia", "mia's forbidden username is skipped");
	assert!(!exists(services, "forbidden_mia").await?, "no forbidden account is created");

	let noah = SignIn {
		sub: "noah",
		username: "bad!name",
		mailbox: "noah",
		..mia
	};

	let response = sign_in_to(client, base, noah).await?;
	let account = signed_in(services, &response).await?;

	assert_eq!(account.localpart(), "noah", "noah's malformed username is skipped");

	let paul = SignIn {
		sub: "paul",
		username: "@paul:elsewhere",
		mailbox: "paul",
		..mia
	};

	let response = sign_in_to(client, base, paul).await?;
	let account = signed_in(services, &response).await?;

	assert_eq!(account.localpart(), "paul", "paul's foreign user ID is skipped");

	let quinn = SignIn {
		sub: "quinn",
		username: "@forbidden_quinn:localhost",
		mailbox: "quinn",
		..mia
	};

	let response = sign_in_to(client, base, quinn).await?;
	let account = signed_in(services, &response).await?;

	assert_eq!(account.localpart(), "quinn", "quinn's forbidden localpart is skipped");
	assert!(!exists(services, "forbidden_quinn").await?, "no forbidden account is created");

	let leo = SignIn {
		sub: "leo",
		username: "shared_leo",
		mailbox: "leo",
		..mia
	};

	let response = sign_in_to(client, base, leo).await?;
	let account = signed_in(services, &response).await?;

	assert_eq!(account.localpart(), "shared_leo", "a shared namespace reserves nothing");

	let kate = SignIn {
		idp: TRUSTED,
		sub: "kate",
		username: "names",
		mailbox: "names",
		..mia
	};

	let response = sign_in_to(client, base, kate).await?;
	let account = signed_in(services, &response).await?;
	let trusted = services.oauth.providers.get(TRUSTED).await?;
	let kate_identity = unique_id_sub((&trusted, "kate"))?;

	assert!(exists(services, "names").await?, "the appservice's sender has an account");
	assert_eq!(fallback_localpart(&kate_identity), account.localpart(), "kate gets her own");

	let olivia = SignIn {
		idp: STRICT,
		sub: "olivia",
		username: "reserved_olivia",
		mailbox: "reserved_olivia",
		..mia
	};

	let response = sign_in_to(client, base, olivia).await?;

	assert_eq!(response.status(), StatusCode::BAD_REQUEST);

	let error: Value = response.json().await?;
	let strict = services.oauth.providers.get(STRICT).await?;
	let olivia_identity = unique_id_sub((&strict, "olivia"))?;
	let fallback = fallback_localpart(&olivia_identity);
	let linked = services
		.oauth
		.sessions
		.get_by_unique_id(&olivia_identity)
		.await;

	assert_eq!(error["errcode"], "M_USER_IN_USE");
	assert!(linked.is_not_found(), "olivia's identity is linked to no account");
	assert!(!exists(services, "reserved_olivia").await?, "no reserved account is created");
	assert!(!exists(services, &fallback).await?, "the disabled fallback creates no account");

	Ok(())
}

async fn register_shared(services: &Services) -> Result {
	let namespaces = Namespaces {
		users: vec![Namespace::new(false, "^@shared_.*:localhost$".to_owned())],
		..Default::default()
	};

	let registration: Registration = RegistrationInit {
		id: "shared".to_owned(),
		url: None,
		as_token: "test-shared-token".to_owned(),
		hs_token: "test-shared-hs-token".to_owned(),
		sender_localpart: "shared".to_owned(),
		namespaces,
		rate_limited: None,
		protocols: None,
	}
	.into();

	services
		.appservice
		.register_appservice(registration)
		.await
}

async fn sign_in(
	client: &Client,
	base: &str,
	callback: &str,
	sub: &str,
	username: &str,
) -> Result<Response> {
	let sign_in = SignIn {
		idp: IDP,
		callback,
		destination: CLIENT,
		sub,
		username,
		mailbox: username,
	};

	sign_in_to(client, base, sign_in).await
}

/// Sign in at provider `idp` as `sub` claiming `username`, bound for `destination`.
///
/// The provider also reports an address at `mailbox` and returns to the server
/// at `callback`. The callback's response returns unfollowed, so the caller
/// reads its status and where it redirects.
async fn sign_in_to(
	client: &Client,
	base: &str,
	SignIn {
		idp,
		callback,
		destination,
		sub,
		username,
		mailbox,
	}: SignIn<'_>,
) -> Result<Response> {
	let url = format!("{base}/_matrix/client/v3/login/sso/redirect/{idp}");
	let redirect = client
		.get(url)
		.query(&[("redirectUrl", destination)])
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

	let url = format!("{base}/{callback}/{idp}");
	let code = format!("{sub}.{username}.{mailbox}");

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

/// Assert that a finished sign-in's nonces and tokens reached no log output.
///
/// The stand-in's access token is the callback's code, whose encoded and decoded
/// spellings match, so that entry covers the code in a URL as well. A release
/// build compiles out the span showing the capture ran.
fn assert_unlogged(session: &Session) {
	let logs = String::from_utf8_lossy(&LOGS.lock().expect("log capture")).into_owned();
	let credentials = [
		("cookie_nonce", &session.cookie_nonce),
		("query_nonce", &session.query_nonce),
		("code_verifier", &session.code_verifier),
		("access_token", &session.access_token),
		("refresh_token", &session.refresh_token),
		("id_token", &session.id_token),
	];

	assert!(
		STATIC_MAX_LEVEL < LevelFilter::DEBUG || logs.contains("sso_callback"),
		"the capture should have seen the callback's span"
	);

	for (name, credential) in credentials {
		let credential = credential.as_deref().expect(name);
		let leak = logs
			.lines()
			.find(|line| line.contains(credential));

		assert_eq!(leak, None, "{name} reached the log output");
	}
}

async fn exists(services: &Services, localpart: &str) -> Result<bool> {
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
