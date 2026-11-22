#![cfg(test)]

use std::{future::ready, pin::pin, time::Duration};

use const_str::concat;
use futures::{
	channel::oneshot::{Receiver, Sender, channel},
	future::{Either, join, select},
};
use reqwest::{Response, StatusCode};
use serde_json::Value;
use tokio::time::{sleep, timeout};
use tuwunel_core::{
	Err, Result, err,
	ruma::{DeviceId, OwnedUserId, UserId, api::error::ErrorKind, device_id},
	utils::time::now_secs,
};
use tuwunel_service::{
	Services,
	users::device::{REFRESH_TOKEN_PREFIX, RefreshToken, generate_refresh_token},
};

use self::{
	appservice::{Bridge, register_appservice},
	fixture::boot,
};

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

mod appservice;
mod fixture;

/// How long a request contending for a held device lock is watched for
/// finishing early.
const CONTENTION: Duration = Duration::from_millis(500);

/// The access token an exchange offered, and the refresh token it returned.
///
/// The access token comes back even when the exchange is refused, so a refusal
/// can be checked for having issued nothing.
type Exchange = (String, Result<String>);

#[test]
fn refresh_device_lock() -> Result {
	let quiet = "logging.enable=false";
	let grace = "authentication.refresh_token.reuse_grace=3600";
	let ttl = "authentication.refresh_token.ttl=2";
	let absolute = "authentication.refresh_token.idle_only=false";

	boot("refresh-device-lock", [grace, quiet], with_grace)?;
	boot(
		"refresh-reuse",
		["authentication.refresh_token.reuse_grace=0", quiet],
		reuse_without_grace,
	)?;

	boot("refresh-soft-expiry", [ttl, absolute, grace, quiet], soft_expiry)?;
	boot(
		"refresh-hard-expiry",
		[ttl, absolute, grace, "authentication.refresh_token.hard_logout=true", quiet],
		hard_expiry,
	)
}

async fn with_grace(services: &Services, base: &str) -> Result {
	removed_device_is_issued_no_tokens(services).await?;
	concurrent_refreshes_keep_one_successor(services).await?;
	stale_grace_is_refused(services).await?;
	grace_without_successor_is_refused(services).await?;
	refused_gate_spends_nothing(services).await?;
	contenders_wait_for_the_device_lock(services).await?;
	expiring_login_refuses_a_refresh_in_flight(services).await?;
	expiring_login_retires_the_refresh_it_waited_for(services).await?;
	metadata_updates_keep_removal(services).await?;
	issuing_forgets_older_expired_tokens(services).await?;
	refresh_token_is_no_bearer(services, base).await?;
	no_refresh_string_is_a_bearer(services, base).await?;
	expired_access_token_keeps_soft_logout(services, base).await?;
	refresh_token_still_revokes(services, base).await?;
	prefixed_appservice_token_authenticates(services, base).await
}

/// A refresh or login still in flight when its device is removed must not
/// leave tokens behind for the removed device.
///
/// The refresh classified its token before the removal, and the login issues
/// to the device ID it was given.
async fn removed_device_is_issued_no_tokens(services: &Services) -> Result {
	let device = device_id!("REMOVEDDEVICE");
	let (user, original, seen) = classified_session(services, "removed", device).await?;

	services.users.remove_device(&user, device).await;

	let (access, refreshed) = exchange(services, &seen, &original).await?;

	if !seen.exchangeable() || refreshed.is_ok() || resolves(services, &access).await {
		return Err!("a refresh issued tokens to a removed device");
	}

	let (access, expires_in) = services.users.generate_access_token(true);
	let refresh = generate_refresh_token();
	let issued = services
		.users
		.set_access_token(&user, device, &access, expires_in, Some(&refresh))
		.await;

	if issued.is_ok() || resolves(services, &access).await || resolves(services, &refresh).await {
		return Err!("a login issued tokens to a removed device");
	}

	Ok(())
}

/// Two refreshes that both found the same refresh token current rotate it
/// once: the later one keeps the earlier one's successor.
///
/// Both exchanges stand for requests that classified the original token before
/// either took the device lock.
async fn concurrent_refreshes_keep_one_successor(services: &Services) -> Result {
	let device = device_id!("CONCURRENTDEVICE");
	let (_, original, seen) = classified_session(services, "concurrent", device).await?;
	let (_, first) = exchange(services, &seen, &original).await?;
	let (access, second) = exchange(services, &seen, &original).await?;

	if first? != second? {
		return Err!("the refresh token was rotated twice");
	}

	if !resolves(services, &access).await {
		return Err!("the later refresh was issued no access token");
	}

	Ok(())
}

/// A refresh that found its token spent inside the grace window is refused
/// once another refresh has rotated the successor it would have kept.
///
/// Only one spent generation is retained, so by then the token is
/// unrecognized, and the device's newest token stays current.
async fn stale_grace_is_refused(services: &Services) -> Result {
	let device = device_id!("STALEGRACEDEVICE");
	let (_, original) = session(services, "stalegrace", device).await?;
	let successor = refreshed_token(services, &original).await?;
	let seen = classify(services, &original).await;
	let latest = refreshed_token(services, &successor).await?;
	let (access, kept) = exchange(services, &seen, &original).await?;

	if !seen.exchangeable()
		|| !refused_with(&kept, &ErrorKind::forbidden())
		|| resolves(services, &access).await
	{
		return Err!("a stale grace classification was exchanged");
	}

	if !is_current(services, &latest).await {
		return Err!("the refused refresh disturbed the device's current token");
	}

	Ok(())
}

/// A spent token presented again inside the grace window is refused as
/// unrecognized once its successor no longer resolves, as presenting the
/// successor is.
///
/// Losing the successor's token row leaves the device pointing at it, and the
/// refusals leave the device in place.
async fn grace_without_successor_is_refused(services: &Services) -> Result {
	let device = device_id!("RETIREDDEVICE");
	let (user, original) = session(services, "retired", device).await?;
	let successor = refreshed_token(services, &original).await?;

	services.db["token_userdeviceid"].remove(&successor);

	let seen = classify(services, &original).await;
	let (access, replay) = exchange(services, &seen, &original).await?;
	let (_, successor_outcome) = refresh(services, &successor).await?;

	if !matches!(seen, RefreshToken::Unknown)
		|| !refused_with(&replay, &ErrorKind::forbidden())
		|| !refused_with(&successor_outcome, &ErrorKind::forbidden())
		|| resolves(services, &access).await
		|| !services.users.device_exists(&user, device).await
	{
		return Err!("a grace replay whose successor no longer resolves was not refused alone");
	}

	Ok(())
}

/// An account check that refuses the exchange leaves the token as it was, for
/// a current token and for a spent one inside the grace window alike.
///
/// The check runs under the device lock after the token is classified again,
/// and its error is the exchange's.
async fn refused_gate_spends_nothing(services: &Services) -> Result {
	let device = device_id!("GATEDDEVICE");
	let (_, original) = session(services, "gated", device).await?;

	if !locked_out(services, &original, &original).await? {
		return Err!("a refused account check let a current token through or spent it");
	}

	let successor = refreshed_token(services, &original).await?;

	if !locked_out(services, &original, &successor).await? {
		return Err!("a refused account check let a spent token through or lost its successor");
	}

	Ok(())
}

/// Whether an exchange of `presented` behind a locked account was refused
/// with the lock's error and left `current` as the device's current token.
async fn locked_out(services: &Services, presented: &str, current: &str) -> Result<bool> {
	let seen = classify(services, presented).await;
	let locked = ready(Err!(Request(UserLocked("locked"))));
	let (access, gated) = exchange_gated(services, &seen, presented, locked).await?;

	Ok(refused_with(&gated, &ErrorKind::UserLocked)
		&& !resolves(services, &access).await
		&& is_current(services, current).await)
}

/// A metadata update and a removal arriving while a refresh holds the device
/// lock both wait for it, and the removal then takes the refresh's tokens.
///
/// The refresh holds the lock through its account check, which is released
/// only after both have been watched for `CONTENTION`.
async fn contenders_wait_for_the_device_lock(services: &Services) -> Result {
	let device = device_id!("HELDDEVICE");
	let (user, original, seen) = classified_session(services, "held", device).await?;
	let (gate, reached_gate, release) = parked_gate();
	let refresh = exchange_gated(services, &seen, &original, gate);
	let contenders = async {
		let reached = reached_gate.await.is_ok();
		let update = pin!(
			services
				.users
				.update_device_last_seen(&user, device, None, None)
		);

		let removal = pin!(services.users.remove_device(&user, device));
		let (update_waited, update) = contend(update, CONTENTION).await;
		let (removal_waited, removal) = contend(removal, CONTENTION).await;

		release.send(()).ok();

		let updated = update.await;

		removal.await;

		(reached && update_waited && removal_waited, updated)
	};

	let (exchanged, (waited, updated)) = join(refresh, contenders).await;
	let (access, refreshed) = exchanged?;

	if !waited || refreshed.is_err() || updated.is_err() {
		return Err!("an update or removal did not wait for the refresh holding the device lock");
	}

	if resolves(services, &access).await || services.users.device_exists(&user, device).await {
		return Err!("the removal waiting behind a refresh left its tokens or its device");
	}

	Ok(())
}

/// An account check that holds its exchange under the device lock until
/// released.
///
/// Returns the check, a signal that the exchange reached it, and its release.
fn parked_gate() -> (impl Future<Output = Result> + Send, Receiver<()>, Sender<()>) {
	let (at_gate, reached_gate) = channel();
	let (release, released) = channel();
	let gate = async move {
		at_gate
			.send(())
			.expect("the contenders stopped watching the account check");

		released
			.await
			.map_err(|_| err!("the account check was never released"))
	};

	(gate, reached_gate, release)
}

/// Watches a request contending for a lock for `window`.
///
/// Returns whether it was still waiting by then, and a future finishing it
/// either way, so one that skipped the lock is reported rather than polled
/// again after completing.
async fn contend<Contender>(
	contender: Contender,
	window: Duration,
) -> (bool, impl Future<Output = Contender::Output>)
where
	Contender: Future + Unpin,
{
	match select(contender, pin!(sleep(window))).await {
		| Either::Left((output, _)) => (false, Either::Left(ready(output))),
		| Either::Right(((), contender)) => (true, Either::Right(contender)),
	}
}

/// A refresh still in flight when an expiring login retires its token is
/// refused, and the login's access token stands.
///
/// The refresh classified the token before the login, which issues no refresh
/// token of its own.
async fn expiring_login_refuses_a_refresh_in_flight(services: &Services) -> Result {
	let device = device_id!("INFLIGHTDEVICE");
	let (user, original, seen) = classified_session(services, "inflight", device).await?;
	let (access, expires_in) = services.users.generate_access_token(true);

	services
		.users
		.set_access_token(&user, device, &access, expires_in, None)
		.await?;

	let (offered, refreshed) = exchange(services, &seen, &original).await?;

	if !seen.exchangeable()
		|| !refused_with(&refreshed, &ErrorKind::forbidden())
		|| resolves(services, &offered).await
		|| resolves(services, &original).await
	{
		return Err!("a refresh outlived the expiring login that retired its token");
	}

	if !resolves(services, &access).await {
		return Err!("the expiring login was issued no access token");
	}

	Ok(())
}

/// An expiring login arriving while a refresh holds the device lock waits for
/// it, then retires the token that refresh issued.
///
/// The refresh holds the lock through its account check, which is released
/// only after the login has been watched for `CONTENTION`.
async fn expiring_login_retires_the_refresh_it_waited_for(services: &Services) -> Result {
	let device = device_id!("WAITINGDEVICE");
	let (user, original, seen) = classified_session(services, "waiting", device).await?;
	let (gate, reached_gate, release) = parked_gate();
	let (access, expires_in) = services.users.generate_access_token(true);
	let refresh = exchange_gated(services, &seen, &original, gate);
	let login = async {
		let reached = reached_gate.await.is_ok();
		let login = services
			.users
			.set_access_token(&user, device, &access, expires_in, None);

		let login = pin!(login);
		let (waited, login) = contend(login, CONTENTION).await;

		release.send(()).ok();

		(reached && waited, login.await)
	};

	let (exchanged, (waited, login)) = join(refresh, login).await;
	let (_, refreshed) = exchanged?;

	login?;

	if !waited {
		return Err!("the expiring login did not wait for the refresh holding the device lock");
	}

	if resolves(services, &refreshed?).await {
		return Err!("the token of a refresh ahead of an expiring login outlived it");
	}

	Ok(())
}

/// A rename is announced to the device list only when the name changes, and no
/// metadata update writes a removed device back.
///
/// Updates read and write the device under its lock, so one arriving after the
/// removal finds no device and fails.
async fn metadata_updates_keep_removal(services: &Services) -> Result {
	let device = device_id!("RENAMEDDEVICE");
	let (user, _) = session(services, "renamed", device).await?;
	let created = services
		.users
		.get_devicelist_version(&user)
		.await?;

	services
		.users
		.rename_device(&user, device, "phone")
		.await?;

	let renamed = services
		.users
		.get_devicelist_version(&user)
		.await?;

	services
		.users
		.rename_device(&user, device, "phone")
		.await?;

	let repeated = services
		.users
		.get_devicelist_version(&user)
		.await?;

	if renamed == created || repeated != renamed {
		return Err!("a rename was announced without changing the name, or not at all");
	}

	services.users.remove_device(&user, device).await;

	let seen = services
		.users
		.update_device_last_seen(&user, device, None, None)
		.await;

	let renamed = services
		.users
		.rename_device(&user, device, "tablet")
		.await;

	if seen.is_ok() || renamed.is_ok() || services.users.device_exists(&user, device).await {
		return Err!("a metadata update wrote a removed device back");
	}

	Ok(())
}

/// Issuing a token forgets the device's expired tokens except those that
/// expired last and the one it replaces, which its client may still be
/// presenting.
///
/// Tokens not yet expired, or without an expiry, stay.
async fn issuing_forgets_older_expired_tokens(services: &Services) -> Result {
	let device = device_id!("EXPIRINGDEVICE");
	let (user, _) = session(services, "expiring", device).await?;
	let now = now_secs();
	let issue = async |expires_at| issue_expiring_at(services, &user, device, expires_at).await;

	let lasting = issue(None).await?;
	let unexpired = issue(Some(now.saturating_add(3600))).await?;
	let last = issue(Some(now.saturating_sub(60))).await?;
	let twin = issue(Some(now.saturating_sub(60))).await?;
	let older = issue(Some(now.saturating_sub(120))).await?;
	let replaced = issue(Some(now.saturating_sub(180))).await?;
	let issued = issue(None).await?;

	if resolves(services, &older).await || indexed(services, &user, device, &older).await {
		return Err!("an expired token outlived a later token expiring");
	}

	for token in [&lasting, &unexpired, &last, &twin, &replaced, &issued] {
		if !resolves(services, token).await || !indexed(services, &user, device, token).await {
			return Err!("issuing a token forgot a token it should have kept");
		}
	}

	Ok(())
}

async fn issue_expiring_at(
	services: &Services,
	user: &UserId,
	device: &DeviceId,
	expires_at: Option<u64>,
) -> Result<String> {
	let (token, _) = services.users.generate_access_token(false);

	services
		.users
		.set_access_token(user, device, &token, None, None)
		.await?;

	services.db["token_userdeviceid"].raw_put(&token, (user, device, expires_at));

	Ok(token)
}

async fn indexed(services: &Services, user: &UserId, device: &DeviceId, token: &str) -> bool {
	services.db["userdeviceidtoken_index"]
		.qry(&(user, device, token))
		.await
		.is_ok()
}

/// A refresh token authenticates no request as a bearer token, on the client
/// API or at OIDC userinfo, and still rotates after both refusals.
///
/// The access token issued by the same exchange is the control at each
/// endpoint, so a refusal is the token's kind and not its device's.
async fn refresh_token_is_no_bearer(services: &Services, base: &str) -> Result {
	let device = device_id!("BEARERDEVICE");
	let (user, original) = session(services, "bearer", device).await?;
	let (access, successor) = refresh(services, &original).await?;
	let successor = successor?;

	services.users.mark_oidc_device(&user, device, "");

	let whoami = whoami_url(base);
	let refused = get_as(services, &whoami, &successor).await?;

	if unknown_token(refused).await? {
		return Err!("a refresh token refused as a bearer token asked for a soft logout");
	}

	let accepted = get_as(services, &whoami, &access).await?;

	identifies(accepted, "user_id", &user).await?;

	let userinfo = userinfo_url(base);
	let status = get_as(services, &userinfo, &successor)
		.await?
		.status();

	if status != StatusCode::UNAUTHORIZED {
		return Err!("expected 401 at userinfo for a refresh token, got {status}");
	}

	let accepted = get_as(services, &userinfo, &access).await?;

	identifies(accepted, "sub", &user).await?;

	if let Err(error) = refreshed_token(services, &successor).await {
		return Err!("a refresh token refused as a bearer token stopped rotating: {error}");
	}

	Ok(())
}

fn whoami_url(base: &str) -> String { format!("{base}/_matrix/client/v3/account/whoami") }

async fn get_as(services: &Services, url: &str, token: &str) -> Result<Response> {
	let response = services
		.client
		.clients
		.default
		.get(url)
		.bearer_auth(token)
		.send()
		.await?;

	Ok(response)
}

/// Checks that a request was refused with `401 M_UNKNOWN_TOKEN`.
///
/// Returns whether the refusal asked for a soft logout.
async fn unknown_token(response: Response) -> Result<bool> {
	let status = response.status();
	let body: Value = response.json().await?;

	if status != StatusCode::UNAUTHORIZED || body["errcode"] != "M_UNKNOWN_TOKEN" {
		return Err!("expected 401 M_UNKNOWN_TOKEN, got {status}: {body}");
	}

	Ok(body["soft_logout"] == true)
}

/// Checks that a request was accepted for `user`, whom the reply names in
/// `field`.
async fn identifies(response: Response, field: &str, user: &UserId) -> Result {
	let status = response.status();
	let body: Value = response.json().await?;

	if status != StatusCode::OK || body[field] != user.as_str() {
		return Err!("expected 200 naming {user} in {field}, got {status}: {body}");
	}

	Ok(())
}

fn userinfo_url(base: &str) -> String { format!("{base}/_tuwunel/oidc/userinfo") }

/// No other refresh-shaped string authenticates as a bearer token.
async fn no_refresh_string_is_a_bearer(services: &Services, base: &str) -> Result {
	let device = device_id!("STRINGSDEVICE");
	let (_, spent) = session(services, "strings", device).await?;

	refreshed_token(services, &spent).await?;

	let whoami = whoami_url(base);
	let unknown = generate_refresh_token();

	for token in ["", REFRESH_TOKEN_PREFIX, unknown.as_str(), spent.as_str()] {
		let status = get_as(services, &whoami, token).await?.status();

		if status != StatusCode::UNAUTHORIZED {
			return Err!("the bearer {token:?} was not refused: {status}");
		}
	}

	Ok(())
}

/// An expired access token still reads as a soft logout on the client API,
/// while OIDC userinfo refuses it.
///
/// Userinfo accepts a lasting token issued to the same device afterwards, so
/// expiry alone is the reason for its refusal.
async fn expired_access_token_keeps_soft_logout(services: &Services, base: &str) -> Result {
	let device = device_id!("EXPIREDDEVICE");
	let (user, _) = session(services, "expired", device).await?;
	let deadline = now_secs().saturating_sub(60);
	let expired = issue_expiring_at(services, &user, device, Some(deadline)).await?;

	services.users.mark_oidc_device(&user, device, "");

	let refused = get_as(services, &whoami_url(base), &expired).await?;

	if !unknown_token(refused).await? {
		return Err!("an expired access token was refused without a soft logout");
	}

	let userinfo = userinfo_url(base);
	let status = get_as(services, &userinfo, &expired)
		.await?
		.status();

	if status != StatusCode::UNAUTHORIZED {
		return Err!("expected 401 at userinfo for an expired access token, got {status}");
	}

	let lasting = issue_expiring_at(services, &user, device, None).await?;
	let accepted = get_as(services, &userinfo, &lasting).await?;

	identifies(accepted, "sub", &user).await
}

/// Revoking a refresh token also ends the access token issued alongside it.
///
/// The access token is checked to work first, so its refusal afterwards is the
/// revocation's doing.
async fn refresh_token_still_revokes(services: &Services, base: &str) -> Result {
	let device = device_id!("REVOKEDDEVICE");
	let (user, original) = session(services, "revoked", device).await?;
	let (access, successor) = refresh(services, &original).await?;
	let successor = successor?;
	let whoami = whoami_url(base);
	let accepted = get_as(services, &whoami, &access).await?;

	identifies(accepted, "user_id", &user).await?;

	let revoke = format!("{base}/_tuwunel/oidc/revoke");

	let status = services
		.client
		.clients
		.default
		.post(&revoke)
		.form(&[("token", successor.as_str())])
		.send()
		.await?
		.status();

	if status != StatusCode::OK {
		return Err!("expected 200 from revocation, got {status}");
	}

	let refused = get_as(services, &whoami, &access).await?;

	if unknown_token(refused).await? {
		return Err!("a revoked access token asked for a soft logout");
	}

	if services.users.device_exists(&user, device).await || resolves(services, &successor).await {
		return Err!("revoking a refresh token left its session");
	}

	Ok(())
}

/// An appservice token that starts like a refresh token still authenticates
/// its appservice.
///
/// The prefix check belongs to the user token lookup alone; an appservice
/// token is matched against its registration.
async fn prefixed_appservice_token_authenticates(services: &Services, base: &str) -> Result {
	let server_name = services.globals.server_name();
	let token = concat!(REFRESH_TOKEN_PREFIX, "appservice");
	let users = format!("@prefixed_.*:{server_name}");
	let bridge = Bridge {
		id: "prefixed",
		token,
		sender_localpart: "prefixedbot",
		users: &users,
		aliases: None,
	};

	register_appservice(services, &bridge).await?;

	let sender = UserId::parse_with_server_name(bridge.sender_localpart, server_name)?;
	let accepted = get_as(services, &whoami_url(base), token).await?;

	identifies(accepted, "user_id", &sender).await
}

/// Without a grace window, a token already spent when its request arrived is
/// reused, so its device is revoked, while one spent by a concurrent exchange
/// as its request waited is refused alone.
///
/// The first refusal stands for a request that found the token current before
/// the rotation took the device lock.
async fn reuse_without_grace(services: &Services, _base: &str) -> Result {
	let device = device_id!("REUSEDEVICE");
	let (user, original, seen) = classified_session(services, "reuse", device).await?;
	let (_, successor) = exchange(services, &seen, &original).await?;
	let successor = successor?;
	let (access, waited) = exchange(services, &seen, &original).await?;

	if !refused_with(&waited, &ErrorKind::forbidden())
		|| resolves(services, &access).await
		|| !is_current(services, &successor).await
	{
		return Err!("a refresh token spent while its request waited was exchanged or revoked");
	}

	let (access, reused) = refresh(services, &original).await?;

	if !refused(&reused, false)
		|| resolves(services, &access).await
		|| resolves(services, &successor).await
		|| services.users.device_exists(&user, device).await
	{
		return Err!("a reused refresh token left its device in place");
	}

	Ok(())
}

async fn soft_expiry(services: &Services, _base: &str) -> Result {
	expiry_while_waiting(services, false).await?;
	grace_after_deadline(services, false).await
}

async fn hard_expiry(services: &Services, _base: &str) -> Result {
	expiry_while_waiting(services, true).await?;
	grace_after_deadline(services, true).await
}

/// A refresh that found its token current is refused when the token has
/// expired by the time it holds the device lock.
///
/// Soft logout retires only the refresh token and hard logout the device.
async fn expiry_while_waiting(services: &Services, hard: bool) -> Result {
	let device = device_id!("EXPIRYDEVICE");
	let (_, original, seen) = classified_session(services, "expiry", device).await?;
	let exchangeable = seen.exchangeable();

	outlast_ttl(services).await;

	if !exchangeable || !logged_out(services, &seen, &original, &original, hard).await? {
		return Err!("an expired refresh token was exchanged or left in place");
	}

	Ok(())
}

/// A spent token presented again inside the grace window is refused with the
/// configured logout once the deadline its successor inherited has passed.
///
/// Before the deadline the same replay keeps the successor.
async fn grace_after_deadline(services: &Services, hard: bool) -> Result {
	let device = device_id!("DEADLINEDEVICE");
	let (_, original) = session(services, "deadline", device).await?;
	let successor = refreshed_token(services, &original).await?;
	let replayed = refreshed_token(services, &original).await?;

	if replayed != successor {
		return Err!("a grace replay before the deadline did not keep the successor");
	}

	outlast_ttl(services).await;

	let seen = classify(services, &original).await;

	if !matches!(seen, RefreshToken::Replayed { grace: true, .. })
		|| seen.exchangeable()
		|| !logged_out(services, &seen, &original, &successor, hard).await?
	{
		return Err!("a grace replay past the deadline was exchanged or left in place");
	}

	Ok(())
}

/// Waits until a refresh token issued before the call has expired.
///
/// The wait outlasts the configured `authentication.refresh_token.ttl`.
async fn outlast_ttl(services: &Services) {
	let ttl = services
		.server
		.config
		.authentication
		.refresh_token
		.ttl;

	sleep(Duration::from_secs(ttl.saturating_add(1))).await;
}

/// Exchanges `presented`, classified as `seen`, and reports whether it was
/// refused with the logout `hard` selects.
///
/// Neither the access token it offered nor `retired` may resolve afterwards,
/// and the device remains only under soft logout.
async fn logged_out(
	services: &Services,
	seen: &RefreshToken,
	presented: &str,
	retired: &str,
	hard: bool,
) -> Result<bool> {
	let Some((user, device)) = seen.owner() else {
		return Ok(false);
	};

	let (access, outcome) = exchange(services, seen, presented).await?;
	let kept = services.users.device_exists(user, device).await;
	let refused_and_retired = refused(&outcome, !hard)
		&& !resolves(services, &access).await
		&& !resolves(services, retired).await
		&& kept != hard;

	Ok(refused_and_retired)
}

async fn session(
	services: &Services,
	localpart: &str,
	device: &DeviceId,
) -> Result<(OwnedUserId, String)> {
	let user = UserId::parse_with_server_name(localpart, services.globals.server_name())?;
	let refresh = generate_refresh_token();
	let (access, expires_in) = services.users.generate_access_token(true);

	services.users.create(&user, None, None).await?;
	services
		.users
		.create_device(
			&user,
			Some(device),
			(Some(&access), expires_in),
			Some(&refresh),
			None,
			None,
		)
		.await?;

	Ok((user, refresh))
}

/// Opens a session and classifies its refresh token, for a request that arrived
/// before whatever the scenario does next.
async fn classified_session(
	services: &Services,
	localpart: &str,
	device: &DeviceId,
) -> Result<(OwnedUserId, String, RefreshToken)> {
	let (user, original) = session(services, localpart, device).await?;
	let seen = classify(services, &original).await;

	Ok((user, original, seen))
}

/// Classifies a refresh token as the token endpoints do when a request
/// arrives.
async fn classify(services: &Services, token: &str) -> RefreshToken {
	services.users.classify_refresh_token(token).await
}

/// Refreshes `presented` and returns the refresh token the exchange keeps.
///
/// The access token it offered is dropped, and a refused exchange fails the
/// call.
async fn refreshed_token(services: &Services, presented: &str) -> Result<String> {
	refresh(services, presented)
		.await
		.and_then(|(_, kept)| kept)
}

/// Exchanges a refresh token for a request that classifies it on arrival, with
/// nothing in between.
async fn refresh(services: &Services, presented: &str) -> Result<Exchange> {
	let seen = classify(services, presented).await;

	exchange(services, &seen, presented).await
}

async fn exchange(services: &Services, seen: &RefreshToken, presented: &str) -> Result<Exchange> {
	exchange_gated(services, seen, presented, ready(Ok(()))).await
}

/// Exchanges a refresh token as the token endpoints do for a request that
/// classified it as `seen`, behind the given account check.
///
/// A device lock the exchange never releases fails the test instead of
/// hanging it.
async fn exchange_gated(
	services: &Services,
	seen: &RefreshToken,
	presented: &str,
	gate: impl Future<Output = Result> + Send,
) -> Result<Exchange> {
	let (access, expires_in) = services.users.generate_access_token(true);
	let exchanged =
		services
			.users
			.rotate_refresh_token(seen, presented, (&access, expires_in), gate);

	let outcome = timeout(Duration::from_secs(10), exchanged)
		.await
		.map_err(|_| err!("the refresh never released the device lock"))?;

	Ok((access, outcome))
}

async fn resolves(services: &Services, token: &str) -> bool {
	services
		.users
		.find_from_token(token)
		.await
		.is_ok()
}

fn refused_with(outcome: &Result<String>, kind: &ErrorKind) -> bool {
	outcome
		.as_ref()
		.is_err_and(|error| error.kind() == *kind)
}

async fn is_current(services: &Services, token: &str) -> bool {
	let token = classify(services, token).await;

	matches!(token, RefreshToken::Current { .. })
}

fn refused(outcome: &Result<String>, soft_logout: bool) -> bool {
	outcome.as_ref().is_err_and(|error| {
		matches!(error.kind(), ErrorKind::UnknownToken(data) if data.soft_logout == soft_logout)
	})
}
