#![cfg(test)]

use std::{future::ready, time::Duration};

use tokio::time::{sleep, timeout};
use tuwunel_core::{
	Err, Result, err,
	ruma::{DeviceId, OwnedUserId, UserId, api::error::ErrorKind, device_id},
};
use tuwunel_service::{
	Services,
	users::device::{RefreshToken, generate_refresh_token},
};

use self::fixture::boot;

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

mod fixture;

/// The access token an exchange offered, and the refresh token it returned.
///
/// The access token comes back even when the exchange is refused, so a refusal
/// can be checked for having issued nothing.
type Exchange = (String, Result<String>);

#[test]
fn refresh_device_lock() -> Result {
	let quiet = "log_enable=false";

	boot("refresh-device-lock", ["refresh_token_reuse_grace=3600", quiet], with_grace)?;
	boot("refresh-reuse", ["refresh_token_reuse_grace=0", quiet], reuse_without_grace)?;
	boot("refresh-soft-expiry", ["refresh_token_ttl=2", quiet], soft_expiry)?;
	boot(
		"refresh-hard-expiry",
		["refresh_token_ttl=2", "refresh_token_hard_logout=true", quiet],
		hard_expiry,
	)
}

async fn with_grace(services: &Services, _base: &str) -> Result {
	removed_device_is_issued_no_tokens(services).await?;
	concurrent_refreshes_keep_one_successor(services).await?;
	stale_grace_is_refused(services).await?;
	refused_gate_spends_nothing(services).await
}

/// A refresh or login still in flight when its device is removed must not
/// leave tokens behind for the removed device.
///
/// The refresh classified its token before the removal, and the login issues
/// to the device ID it was given.
async fn removed_device_is_issued_no_tokens(services: &Services) -> Result {
	let device = device_id!("REMOVEDDEVICE");
	let (user, original) = session(services, "removed", device).await?;
	let seen = classify(services, &original).await;

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
	let (_, original) = session(services, "concurrent", device).await?;
	let seen = classify(services, &original).await;
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
	let (_, successor) = refresh(services, &original).await?;
	let seen = classify(services, &original).await;
	let (_, latest) = refresh(services, &successor?).await?;
	let (access, kept) = exchange(services, &seen, &original).await?;

	if !seen.exchangeable()
		|| !refused_with(&kept, &ErrorKind::forbidden())
		|| resolves(services, &access).await
	{
		return Err!("a stale grace classification was exchanged");
	}

	if !is_current(services, &latest?).await {
		return Err!("the refused refresh disturbed the device's current token");
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

	let (_, successor) = refresh(services, &original).await?;

	if !locked_out(services, &original, &successor?).await? {
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

/// Without a grace window, a token already spent when its request arrived is
/// reused, so its device is revoked, while one spent by a concurrent exchange
/// as its request waited is refused alone.
///
/// The first refusal stands for a request that found the token current before
/// the rotation took the device lock.
async fn reuse_without_grace(services: &Services, _base: &str) -> Result {
	let device = device_id!("REUSEDEVICE");
	let (user, original) = session(services, "reuse", device).await?;
	let seen = classify(services, &original).await;
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
	expiry_while_waiting(services, false).await
}

async fn hard_expiry(services: &Services, _base: &str) -> Result {
	expiry_while_waiting(services, true).await
}

/// A refresh that found its token current is refused when the token has
/// expired by the time it holds the device lock.
///
/// Soft logout retires only the refresh token and hard logout the device. The
/// wait outlasts the configured `refresh_token_ttl`.
async fn expiry_while_waiting(services: &Services, hard: bool) -> Result {
	let device = device_id!("EXPIRYDEVICE");
	let (user, original) = session(services, "expiry", device).await?;
	let seen = classify(services, &original).await;
	let exchangeable = seen.exchangeable();
	let ttl = services.server.config.refresh_token_ttl;

	sleep(Duration::from_secs(ttl.saturating_add(1))).await;

	let (access, expired) = exchange(services, &seen, &original).await?;
	let kept = services.users.device_exists(&user, device).await;

	if !exchangeable
		|| !refused(&expired, !hard)
		|| resolves(services, &access).await
		|| resolves(services, &original).await
		|| kept == hard
	{
		return Err!("an expired refresh token was exchanged or left in place");
	}

	Ok(())
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

/// Classifies a refresh token as the token endpoints do when a request
/// arrives.
async fn classify(services: &Services, token: &str) -> RefreshToken {
	services.users.classify_refresh_token(token).await
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
