#![cfg(test)]
#![cfg(feature = "jwt_login")]

use std::{
	fmt::Display,
	iter::{once, repeat_n},
	net::{IpAddr, Ipv4Addr},
	time::Duration,
};

use futures::{StreamExt, TryStreamExt, future::join_all};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::{Value, json};
use tokio::time::sleep;
use tuwunel_core::{Err, Result, err, itertools::Itertools, ruma::UserId, utils::IterStream};
use tuwunel_service::{Services, users::PASSWORD_SENTINEL};

use self::{
	appservice::{Bridge, register_appservice},
	fixture::boot,
	jwt::mint,
};

mod appservice;
#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;
mod fixture;
mod jwt;

const JWT_SECRET: &str = "jwt-login-deactivated-test-secret";
const PASSWORD: &str = "test-password";
const WRONG_PASSWORD: &str = "not-the-test-password";
const DEACTIVATED: &str = "M_USER_DEACTIVATED";
const LIMITED: &str = "M_LIMIT_EXCEEDED";
const LIMITED_MESSAGE: &str = "Too many password authentication attempts.";
const UNKNOWN_TOKEN: &str = "M_UNKNOWN_TOKEN";
const LOGIN_TOKEN: &str = "jwt-login-deactivated-login-token";
const BRIDGE_TOKEN: &str = "jwt-login-deactivated-bridge-token";
const DEVICE: &str = "JWTLOGINDEVICE";
const UNSEEN_DEVICE: &str = "JWTUNSEENDEVICE";

const BRIDGE: Bridge<'static> = Bridge {
	id: "jwt-login-deactivated",
	token: BRIDGE_TOKEN,
	sender_localpart: "bridge_bot",
	users: "^@bridged_.*$",
	aliases: None,
};

const LASTING: Grant = Grant { lifetime: None, refreshable: false };

/// A sign-in's status, errcode and message.
///
/// A limited answer's retry delay depends on timing, so it is left out.
type Answer = (StatusCode, Value, Value);

#[derive(Clone, Copy, Debug, PartialEq)]
struct Grant {
	/// How long the access token lasts, in milliseconds.
	lifetime: Option<u64>,

	/// Whether a refresh token came with it.
	refreshable: bool,
}

/// Login enforces account state, password limits and the JWT expiry option.
///
/// JWT, login-token and JWT UIAA authentication refuse deactivated accounts,
/// while an appservice can still sign in its passwordless bot. A refused
/// password sign-in gets the same error and uses up the same allowance whatever
/// account, if any, the name belongs to, and an account with no origin recorded
/// still signs in.
///
/// Exhausting one address's failed-password allowance leaves another address
/// and authenticated password re-entry usable, without trusting forged headers.
/// A loopback peer has no address budget, while a non-loopback address shares
/// one budget across accounts and leaves password re-entry its own.
#[test]
fn login_enforces_account_state_password_limits_and_jwt_expiry() -> Result {
	let jwt_key = format!("jwt.key=\"{JWT_SECRET}\"");
	let common = [
		"logging.global_default=false",
		"jwt.enable=true",
		&jwt_key,
		"jwt.format=\"HMAC\"",
		"jwt.algorithm=\"HS256\"",
		"jwt.register_user=false",
		"authentication.argon2.m_cost=64",
		"authentication.argon2.t_cost=1",
	];

	let limits = [
		"rate_limiting.login.failed.per_second=0.000001",
		"rate_limiting.login.address.per_second=0.000001",
	];

	// Expiry floors to whole seconds, so this lifetime leaves a slow runner a margin.
	let expiry = ["jwt.expire_access_token=true", "authentication.access_token_ttl=5"];

	boot("jwt-login-limits", common.into_iter().chain(limits), exercise)?;
	boot("jwt-login-expiry", common.into_iter().chain(expiry), exercise_expiry)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let server_name = services.globals.server_name();
	let active = UserId::parse_with_server_name("active", server_name)?;
	let deactivated = UserId::parse_with_server_name("deactivated", server_name)?;
	let guessed = UserId::parse_with_server_name("guessed", server_name)?;
	let owner = UserId::parse_with_server_name("owner", server_name)?;

	for user_id in [&active, &deactivated, &guessed, &owner] {
		services
			.users
			.create(user_id, Some(PASSWORD), None)
			.await?;
	}

	services
		.users
		.deactivate_account(&deactivated)
		.await?;

	let session = expect_login(services, base, &jwt_auth(&active)?, LASTING).await?;
	let refreshable = Grant {
		lifetime: Some(lifetime_ms(services)),
		refreshable: true,
	};

	expect_login(services, base, &refreshing(jwt_auth(&active)?), refreshable).await?;

	let (status, body) = jwt_login(services, base, &deactivated).await?;

	expect_error("deactivated account", status, &body, StatusCode::FORBIDDEN, DEACTIVATED)?;

	token_login_refuses_deactivated(services, base, &deactivated).await?;
	appservice_login_admits_its_passwordless_bot(services, base).await?;
	jwt_stage_admits_only_active_subjects(services, base, &session, &active, &deactivated)
		.await?;

	let limit = &services.config.rate_limiting.login.failed;
	let burst = limit.burst_count;

	password_refusals_read_alike(services, base, &deactivated, burst).await?;
	legacy_account_signs_in(services, base).await?;
	correct_password_inside_the_burst(services, base, &owner, burst).await?;
	wrong_passwords_are_limited(services, base, &guessed, burst).await
}

async fn token_login_refuses_deactivated(
	services: &Services,
	base: &str,
	deactivated: &UserId,
) -> Result {
	_ = services
		.users
		.create_login_token(deactivated, LOGIN_TOKEN);

	let token_login = json!({"type": "m.login.token", "token": LOGIN_TOKEN});
	let (status, body) = login(services, base, &token_login).await?;

	expect_error("login token", status, &body, StatusCode::FORBIDDEN, DEACTIVATED)
}

async fn appservice_login_admits_its_passwordless_bot(services: &Services, base: &str) -> Result {
	register_appservice(services, &BRIDGE).await?;

	let appservice_login = json!({
		"type": "m.login.application_service",
		"identifier": {"type": "m.id.user", "user": BRIDGE.sender_localpart},
	});

	let request = post(services, base, "login", &appservice_login).bearer_auth(BRIDGE_TOKEN);
	let (status, body) = send(request).await?;

	expect_signed_in("appservice bot login", status, &body)
}

async fn jwt_stage_admits_only_active_subjects(
	services: &Services,
	base: &str,
	session: &Value,
	active: &UserId,
	deactivated: &UserId,
) -> Result {
	let access_token = access_token(session)?;
	let (status, body) =
		change_password_by_jwt(services, base, access_token, deactivated).await?;

	expect_error("JWT stage of UIAA", status, &body, StatusCode::FORBIDDEN, DEACTIVATED)?;

	if !services.users.is_deactivated(deactivated).await? {
		return Err!("the JWT stage of UIAA gave {deactivated} a password again");
	}

	let (status, body) = change_password_by_jwt(services, base, access_token, active).await?;

	if status != StatusCode::OK {
		return Err!("the JWT stage of UIAA refused the active {active}: {status} {body}");
	}

	Ok(())
}

async fn change_password_by_jwt(
	services: &Services,
	base: &str,
	access_token: &str,
	subject: &UserId,
) -> Result<(StatusCode, Value)> {
	let auth = jwt_auth(subject)?;
	let password_change = json!({"new_password": "changed-by-jwt", "auth": auth});
	let request =
		post(services, base, "account/password", &password_change).bearer_auth(access_token);

	send(request).await
}

/// A refused password sign-in gets the same error and uses up the same
/// allowance whatever account, if any, the name belongs to.
///
/// That covers a wrong password, a deactivated account, the server user, an
/// account a JWT login created, and one holding a real password under a
/// provider's origin, given that password. The last two are also tried with
/// a capitalized localpart. Each kind signs in from its own address, since a
/// name in another case shares its account's bucket.
async fn password_refusals_read_alike(
	services: &Services,
	base: &str,
	deactivated: &UserId,
	burst: u32,
) -> Result {
	let server_name = services.globals.server_name();
	let user = |localpart| UserId::parse_with_server_name(localpart, server_name);
	let (unknown, mistyped) = (user("nobody")?, user("mistyped")?);
	let (created, provider) = (user("created")?, user("provider")?);

	services
		.users
		.create(&mistyped, Some(PASSWORD), None)
		.await?;

	services
		.users
		.create(&created, Some(PASSWORD_SENTINEL), Some("jwt"))
		.await?;

	services
		.users
		.create(&provider, Some(PASSWORD), None)
		.await?;

	// No writer pairs a real password with a provider's origin now; an older database may.
	services.db["userid_origin"].insert(&provider, "sso");

	let kinds: [(&UserId, &str); 8] = [
		(&unknown, WRONG_PASSWORD),
		(&mistyped, WRONG_PASSWORD),
		(deactivated, PASSWORD),
		(&services.globals.server_user, PASSWORD),
		(&created, PASSWORD),
		(&user("Created")?, PASSWORD),
		(&provider, PASSWORD),
		(&user("Provider")?, PASSWORD),
	];

	let count = burst.saturating_add(1);
	let answers = kinds
		.into_iter()
		.zip(16..)
		.map(|((user_id, password), host)| attempts(base, host, user_id, password, count));

	let answers: Vec<_> = join_all(answers)
		.await
		.into_iter()
		.try_collect()?;

	let refused = answer(StatusCode::FORBIDDEN, "M_FORBIDDEN", "Invalid username or password.");
	let limited = answer(StatusCode::TOO_MANY_REQUESTS, LIMITED, LIMITED_MESSAGE);
	let expected: Vec<_> = repeat_n(refused, burst.try_into()?)
		.chain(once(limited))
		.collect();

	if !answers.iter().all_equal() {
		return Err!("password refusals differ by account: {answers:#?}");
	}

	if answers.first() != Some(&expected) {
		return Err!("expected {expected:#?} for every account, got {answers:#?}");
	}

	Ok(())
}

/// Signs in `count` times in turn from one loopback address, keeping each
/// answer.
///
/// Each request finishes before the next starts, so the last one meets the
/// allowance the earlier ones left.
async fn attempts(
	base: &str,
	host: u8,
	user_id: &UserId,
	password: &str,
	count: u32,
) -> Result<Vec<Answer>> {
	let client = loopback(host)?;
	let body = password_body(user_id, password);
	let url = format!("{base}/_matrix/client/v3/login");

	(0..count)
		.stream()
		.then(async |_| send(client.post(&url).json(&body)).await)
		.map_ok(|(status, body)| (status, body["errcode"].clone(), body["error"].clone()))
		.try_collect()
		.await
}

fn loopback(host: u8) -> Result<Client> {
	let address = IpAddr::V4(Ipv4Addr::new(127, 0, 0, host));
	let client = Client::builder()
		.no_proxy()
		.local_address(address)
		.build()?;

	Ok(client)
}

// The server prefixes an error's message with its code.
fn answer(status: StatusCode, errcode: &str, message: &str) -> Answer {
	(status, json!(errcode), json!(format!("{errcode}: {message}")))
}

/// An account with no origin recorded still signs in with its password.
///
/// Removing the origin row leaves the account as an older database holds it.
async fn legacy_account_signs_in(services: &Services, base: &str) -> Result {
	let legacy = UserId::parse_with_server_name("legacy", services.globals.server_name())?;

	services
		.users
		.create(&legacy, Some(PASSWORD), None)
		.await?;

	services.db["userid_origin"].remove(&legacy);

	let (status, body) = password_login(services, base, &legacy, PASSWORD).await?;

	expect_signed_in("an account with no origin", status, &body)
}

async fn wrong_passwords_are_limited(
	services: &Services,
	base: &str,
	user_id: &UserId,
	burst: u32,
) -> Result {
	for attempt in 0..burst {
		let (status, body) = password_login(services, base, user_id, WRONG_PASSWORD).await?;
		let context = format_args!("wrong password {attempt}");

		expect_error(context, status, &body, StatusCode::FORBIDDEN, "M_FORBIDDEN")?;
	}

	let (status, body) = password_login(services, base, user_id, WRONG_PASSWORD).await?;
	let limited = StatusCode::TOO_MANY_REQUESTS;

	expect_error("wrong password past the burst", status, &body, limited, LIMITED)?;

	if body["retry_after_ms"].as_u64().is_none() {
		return Err!("the limited refusal stated no retry_after_ms: {body}");
	}

	let (status, body) = password_login(services, base, user_id, PASSWORD).await?;

	expect_error("correct password past the burst", status, &body, limited, LIMITED)?;
	address_isolation(services, base, user_id).await
}

#[tracing::instrument(level = "debug", skip(services))]
async fn address_isolation(services: &Services, base: &str, user_id: &UserId) -> Result {
	let body = password_body(user_id, PASSWORD);
	let forged = post(services, base, "login", &body)
		.header("X-Forwarded-For", "192.0.2.1")
		.header("X-Real-IP", "192.0.2.2")
		.header("Forwarded", "for=192.0.2.3");

	let (status, body) = send(forged).await?;

	expect_error("forged client address", status, &body, StatusCode::TOO_MANY_REQUESTS, LIMITED)?;

	let client = loopback(2)?;
	let body = password_body(user_id, PASSWORD);
	let request = client
		.post(format!("{base}/_matrix/client/v3/login"))
		.json(&body);

	let (status, session) = send(request).await?;

	expect_signed_in("correct password from another address", status, &session)?;
	password_reentry_after_login_limit(services, base, user_id, &session).await?;
	loopback_peer_has_no_address_budget(services, base, user_id).await
}

#[tracing::instrument(level = "debug", skip(services, session))]
async fn password_reentry_after_login_limit(
	services: &Services,
	base: &str,
	user_id: &UserId,
	session: &Value,
) -> Result {
	let access_token = access_token(session)?;
	let auth = password_body(user_id, PASSWORD);
	let body = json!({"new_password": PASSWORD, "logout_devices": false, "auth": auth});
	let request = post(services, base, "account/password", &body).bearer_auth(access_token);
	let (status, body) = send(request).await?;

	if status != StatusCode::OK {
		return Err!("public login exhaustion blocked password UIAA: {status}: {body}");
	}

	Ok(())
}

#[tracing::instrument(level = "debug", skip(services))]
async fn loopback_peer_has_no_address_budget(
	services: &Services,
	base: &str,
	user_id: &UserId,
) -> Result {
	let server_name = services.globals.server_name();
	let untouched = UserId::parse_with_server_name("address-victim", server_name)?;

	services
		.users
		.create(&untouched, Some(PASSWORD), None)
		.await?;

	for attempt in 0..=address_burst(services) {
		let unknown = UserId::parse_with_server_name(format!("spray-{attempt}"), server_name)?;
		let (status, body) = password_login(services, base, &unknown, WRONG_PASSWORD).await?;

		expect_error("loopback spray", status, &body, StatusCode::FORBIDDEN, "M_FORBIDDEN")?;
	}

	let (status, body) = password_login(services, base, &untouched, PASSWORD).await?;

	expect_signed_in("correct password after a loopback spray", status, &body)?;
	address_budget_spans_accounts(services, &untouched, user_id)
}

#[tracing::instrument(level = "debug", skip(services))]
fn address_budget_spans_accounts(
	services: &Services,
	untouched: &UserId,
	user_id: &UserId,
) -> Result {
	let limiter = &services.login_ratelimit;
	let server_name = services.globals.server_name();
	let client = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));

	for attempt in 0..address_burst(services) {
		let unknown = UserId::parse_with_server_name(format!("remote-{attempt}"), server_name)?;

		drop(limiter.reserve_login_attempt(&unknown, client)?);
	}

	let limited = limiter
		.reserve_login_attempt(untouched, client)
		.is_err_and(|error| error.status_code() == StatusCode::TOO_MANY_REQUESTS);

	if !limited {
		return Err!("a remote address kept no budget across accounts");
	}

	let reentry = limiter.reserve_uiaa_attempt(user_id, client)?;

	limiter.refund_login_attempt(reentry)
}

fn address_burst(services: &Services) -> u32 {
	services
		.config
		.rate_limiting
		.login
		.address
		.burst_count
}

async fn correct_password_inside_the_burst(
	services: &Services,
	base: &str,
	user_id: &UserId,
	burst: u32,
) -> Result {
	for attempt in 1..burst {
		let (status, body) = password_login(services, base, user_id, WRONG_PASSWORD).await?;
		let context = format_args!("wrong password {attempt} inside the burst");

		expect_error(context, status, &body, StatusCode::FORBIDDEN, "M_FORBIDDEN")?;
	}

	let (status, body) = password_login(services, base, user_id, PASSWORD).await?;

	expect_signed_in("correct password inside the burst", status, &body)
}

/// Checks that `jwt.expire_access_token` ends a JWT login at the access-token
/// lifetime and leaves a password login alone.
///
/// The JWT login gets no refresh token whether or not the client asks for one,
/// expires into a soft logout, and resumes its device with another login. It
/// retires the refresh tokens its device held, current and spent, which a
/// lasting or refreshing login leaves working, and an expired token can still
/// log out.
async fn exercise_expiry(services: &Services, base: &str) -> Result {
	let server_name = services.globals.server_name();
	let user_id = UserId::parse_with_server_name("expiring", server_name)?;
	let lifetime = Some(lifetime_ms(services));
	let expiring = Grant { lifetime, refreshable: false };
	let refreshable = Grant { lifetime, refreshable: true };

	services
		.users
		.create(&user_id, Some(PASSWORD), None)
		.await?;

	let password_auth = password_body(&user_id, PASSWORD);
	let password_session = expect_login(services, base, &password_auth, LASTING).await?;
	let device_auth = on_device(password_auth, DEVICE);
	let refreshing_auth = refreshing(device_auth.clone());
	let first_session = expect_login(services, base, &refreshing_auth, refreshable).await?;

	expect_login(services, base, &device_auth, LASTING).await?;

	let spent_session = expect_refresh(services, base, &first_session).await?;
	let current_session = expect_login(services, base, &refreshing_auth, refreshable).await?;

	// Inside the reuse grace window a spent token still exchanges until retired.
	expect_refresh(services, base, &spent_session).await?;

	let plain_login = jwt_auth(&user_id)?;
	let leaving = expect_login(services, base, &plain_login, expiring).await?;
	let leaving_all = expect_login(services, base, &plain_login, expiring).await?;

	let device_login = on_device(refreshing(jwt_auth(&user_id)?), DEVICE);
	let session = expect_login(services, base, &device_login, expiring).await?;

	expect_retired(services, base, &spent_session).await?;
	expect_retired(services, base, &current_session).await?;

	let unseen_auth = on_device(password_body(&user_id, PASSWORD), UNSEEN_DEVICE);
	let unseen_session =
		expect_login(services, base, &refreshing(unseen_auth), refreshable).await?;

	// A login that read no device row meets a refresh token another login just gave it.
	services.db["userdeviceid_metadata"].del((&user_id, UNSEEN_DEVICE));

	let unseen_login = on_device(jwt_auth(&user_id)?, UNSEEN_DEVICE);

	expect_login(services, base, &unseen_login, expiring).await?;
	expect_retired(services, base, &unseen_session).await?;

	outlast_access_token(services).await;

	let (status, body) = whoami(services, base, access_token(&password_session)?).await?;

	if status != StatusCode::OK {
		return Err!("the password login expired with the JWT login: {status} {body}");
	}

	let body = expect_unknown_token(services, base, "expired JWT login", &session).await?;

	if body["soft_logout"] != true {
		return Err!("the expired JWT login was not a soft logout: {body}");
	}

	let session = expect_login(services, base, &device_login, expiring).await?;
	let (status, body) = whoami(services, base, access_token(&session)?).await?;

	if status != StatusCode::OK || body["device_id"] != DEVICE {
		return Err!("another JWT login did not resume {DEVICE}: {status} {body}");
	}

	expect_logout(services, base, "logout", &leaving).await?;
	expect_logout(services, base, "logout/all", &leaving_all).await?;

	expect_unknown_token(services, base, "a session outliving logout/all", &password_session)
		.await?;

	Ok(())
}

async fn expect_refresh(services: &Services, base: &str, session: &Value) -> Result<Value> {
	let (status, body) = refresh(services, base, session).await?;

	expect_signed_in("a refresh token before the JWT login", status, &body)?;

	Ok(body)
}

async fn expect_retired(services: &Services, base: &str, session: &Value) -> Result {
	let (status, body) = refresh(services, base, session).await?;

	expect_error("a retired refresh token", status, &body, StatusCode::FORBIDDEN, "M_FORBIDDEN")
}

async fn refresh(
	services: &Services,
	base: &str,
	session: &Value,
) -> Result<(StatusCode, Value)> {
	let body = json!({"refresh_token": session["refresh_token"]});

	send(post(services, base, "refresh", &body)).await
}

async fn expect_logout(services: &Services, base: &str, path: &str, session: &Value) -> Result {
	let token = access_token(session)?;
	let request = post(services, base, path, &json!({})).bearer_auth(token);
	let (status, body) = send(request).await?;

	if status != StatusCode::OK {
		return Err!("{path} with an expired token: expected 200, got {status}: {body}");
	}

	let body = expect_unknown_token(services, base, path, session).await?;

	if body["soft_logout"] == true {
		return Err!("{path} left its expired token recorded: {body}");
	}

	Ok(())
}

async fn expect_unknown_token(
	services: &Services,
	base: &str,
	context: &str,
	session: &Value,
) -> Result<Value> {
	let (status, body) = whoami(services, base, access_token(session)?).await?;

	expect_error(context, status, &body, StatusCode::UNAUTHORIZED, UNKNOWN_TOKEN)?;

	Ok(body)
}

async fn outlast_access_token(services: &Services) {
	let ttl = services.config.authentication.access_token_ttl;

	sleep(Duration::from_secs(ttl.saturating_add(1))).await;
}

async fn whoami(services: &Services, base: &str, token: &str) -> Result<(StatusCode, Value)> {
	let request = services
		.client
		.clients
		.default
		.get(format!("{base}/_matrix/client/v3/account/whoami"))
		.bearer_auth(token);

	send(request).await
}

async fn expect_login(
	services: &Services,
	base: &str,
	body: &Value,
	expected: Grant,
) -> Result<Value> {
	let (status, session) = login(services, base, body).await?;

	expect_signed_in(body, status, &session)?;

	let granted = Grant {
		lifetime: session["expires_in_ms"].as_u64(),
		refreshable: session["refresh_token"].is_string(),
	};

	if granted != expected {
		return Err!("{body}: expected {expected:?}, got {session}");
	}

	Ok(session)
}

fn lifetime_ms(services: &Services) -> u64 {
	services
		.config
		.authentication
		.access_token_ttl
		.saturating_mul(1000)
}

fn refreshing(body: Value) -> Value { with_field(body, "refresh_token", true) }

fn on_device(body: Value, device: &str) -> Value { with_field(body, "device_id", device) }

fn with_field<T>(mut body: Value, key: &str, value: T) -> Value
where
	T: Into<Value>,
{
	body[key] = value.into();

	body
}

fn access_token(session: &Value) -> Result<&str> {
	session["access_token"]
		.as_str()
		.ok_or_else(|| err!("the session has no access token: {session}"))
}

async fn jwt_login(
	services: &Services,
	base: &str,
	user_id: &UserId,
) -> Result<(StatusCode, Value)> {
	login(services, base, &jwt_auth(user_id)?).await
}

fn jwt_auth(user_id: &UserId) -> Result<Value> {
	mint(JWT_SECRET, user_id).map(|token| json!({"type": "org.matrix.login.jwt", "token": token}))
}

async fn password_login(
	services: &Services,
	base: &str,
	user_id: &UserId,
	password: &str,
) -> Result<(StatusCode, Value)> {
	let body = password_body(user_id, password);

	login(services, base, &body).await
}

fn password_body(user_id: &UserId, password: &str) -> Value {
	json!({
		"type": "m.login.password",
		"identifier": {"type": "m.id.user", "user": user_id.localpart()},
		"password": password,
	})
}

async fn login(services: &Services, base: &str, body: &Value) -> Result<(StatusCode, Value)> {
	send(post(services, base, "login", body)).await
}

fn post(services: &Services, base: &str, path: &str, body: &Value) -> RequestBuilder {
	services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/{path}"))
		.json(body)
}

async fn send(request: RequestBuilder) -> Result<(StatusCode, Value)> {
	let response = request.send().await?;
	let status = response.status();
	let body: Value = response.json().await?;

	Ok((status, body))
}

fn expect_signed_in<Context>(context: Context, status: StatusCode, body: &Value) -> Result
where
	Context: Display,
{
	if status != StatusCode::OK || body["access_token"].as_str().is_none() {
		return Err!("{context}: expected 200 with an access token, got {status}: {body}");
	}

	Ok(())
}

fn expect_error<Context>(
	context: Context,
	status: StatusCode,
	body: &Value,
	expected: StatusCode,
	errcode: &str,
) -> Result
where
	Context: Display,
{
	if status != expected || body["errcode"] != errcode {
		return Err!("{context}: expected {expected} {errcode}, got {status}: {body}");
	}

	Ok(())
}
