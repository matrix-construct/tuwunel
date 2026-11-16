//! Browser sessions minted by a password sign-in on the built-in login page.
//!
//! A row records when the browser signed in; expiry is computed at each read
//! from the live `authentication.oidc.browser_session_ttl`, so lowering the
//! setting shortens existing sessions at once. The cookie names the user and a
//! random secret, and only the secret's digest is stored.

use std::time::Duration;

use futures::TryFutureExt;
use ruma::UserId;
use tuwunel_core::{
	Result, debug, implement,
	utils::{
		BoolExt, hash::sha256::hash as sha256, millis_since_unix_epoch, random_string,
		result::NotFound, stream::TryReadyExt, time::now,
	},
	warn,
};
use tuwunel_database::{Deserialized, Interfix};

/// Upper bound on `browser_session_ttl`, in seconds.
///
/// The `useridbrowsersession_createdat` descriptor drops rows at this age, so
/// raising it means raising the descriptor's `ttl` too.
const TTL_MAX: u64 = 60 * 60 * 24 * 90;

const SECRET_LENGTH: usize = 32;

/// A browser session started by a password sign-in.
///
/// The token is the cookie's value and the lifetime its `Max-Age`; neither is
/// stored, since the row keeps only the secret's digest and the sign-in time.
pub struct BrowserSession {
	/// The opaque cookie value naming the session.
	pub token: String,

	/// How long the session lasts from its sign-in.
	pub lifetime: Duration,
}

/// Starts a browser session for a user who just signed in with a password.
///
/// `verified` is the password hash the sign-in checked, `None` when it could
/// not be read. The session is minted only while that hash is still the
/// account's, read under the password lock, so a new password either lands
/// first and refuses the session or lands after and ends it. Disabling a
/// password ends no session, but a deactivated account's sessions are refused
/// at use. The session named by `replaces` ends first, whoever it belongs to,
/// so each sign-in mints a fresh session id and never adopts one the browser
/// already carried. Returns `None` while browser sessions are turned off,
/// without a verified hash, or once the stored hash differs or cannot be read.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, verified, replaces))]
pub async fn create_browser_session(
	&self,
	user_id: &UserId,
	verified: Option<&str>,
	replaces: Option<&str>,
) -> Option<BrowserSession> {
	if let Some((owner, secret)) = replaces.and_then(parse_token) {
		self.remove_browser_session(owner, secret);
	}

	let verified = verified?;
	let _password_guard = self.changing_password.lock(user_id).await;

	self.db
		.userid_password
		.get(user_id)
		.map_ok(|current| &*current == verified.as_bytes())
		.await
		.inspect_err(
			|error| warn!(%user_id, %error, "Unreadable password, starting no browser session"),
		)
		.ok()?
		.and_then(|| self.mint_browser_session(user_id))
}

/// Stores a new browser session for a user and returns its cookie.
///
/// It checks no password, so a sign-in goes through `create_browser_session`.
/// Returns `None` while browser sessions are turned off.
#[implement(super::Service)]
pub(super) fn mint_browser_session(&self, user_id: &UserId) -> Option<BrowserSession> {
	let lifetime = self.browser_session_ttl()?;
	let secret = random_string(SECRET_LENGTH);
	let digest = sha256(&secret);

	self.db
		.useridbrowsersession_createdat
		.put((user_id, digest.as_slice()), millis_since_unix_epoch());

	Some(BrowserSession {
		token: format!("{secret}.{user_id}"),
		lifetime,
	})
}

/// Returns the user a browser session belongs to while the session lasts.
///
/// A garbled, unknown, ended or expired token answers `None`, as does any
/// token while browser sessions are turned off. A read failure other than a
/// missing row is logged and answers `None` as well.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn find_browser_session<'a>(&self, token: &'a str) -> Option<&'a UserId> {
	let lifetime = self.browser_session_ttl()?;
	let Some((user_id, secret)) = parse_token(token) else {
		debug!("Malformed browser session token, ignoring");
		return None;
	};

	let digest = sha256(secret);

	let created_at: u64 = self
		.db
		.useridbrowsersession_createdat
		.qry(&(user_id, digest.as_slice()))
		.await
		.deserialized()
		.present()
		.inspect_err(|error| warn!(%user_id, %error, "Unreadable browser session, ignoring"))
		.ok()
		.flatten()?;

	let age = now().saturating_sub(Duration::from_millis(created_at));

	age.lt(&lifetime).then_some(user_id)
}

/// Ends a browser session when it belongs to `user_id`.
///
/// A token naming another user is left alone, so a request authenticated as
/// one account never signs the browser out of a different one.
#[implement(super::Service)]
pub fn end_browser_session(&self, token: &str, user_id: &UserId) {
	if let Some((owner, secret)) = parse_token(token).filter(|(owner, _)| *owner == user_id) {
		self.remove_browser_session(owner, secret);
	}
}

/// Ends every browser session of a user.
///
/// Password writes, deactivation and signing out of all devices call it. A failed scan is
/// returned rather than skipped, so no caller reports the sessions ended while
/// one survives. The scan stops at the user ID's separator, so a longer user
/// ID sharing its bytes keeps its sessions.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn end_browser_sessions(&self, user_id: &UserId) -> Result {
	let sessions = &self.db.useridbrowsersession_createdat;

	sessions
		.keys_prefix_raw(&(user_id, Interfix))
		.ready_try_for_each(|key| {
			sessions.remove(key);

			Ok(())
		})
		.await
}

fn parse_token(token: &str) -> Option<(&UserId, &str)> {
	let (secret, user_id) = token
		.split_once('.')
		.filter(|(secret, _)| secret.len().eq(&SECRET_LENGTH))?;

	<&UserId>::try_from(user_id)
		.ok()
		.map(|user_id| (user_id, secret))
}

#[implement(super::Service)]
fn remove_browser_session(&self, user_id: &UserId, secret: &str) {
	let digest = sha256(secret);

	self.db
		.useridbrowsersession_createdat
		.del((user_id, digest.as_slice()));
}

#[implement(super::Service)]
fn browser_session_ttl(&self) -> Option<Duration> {
	let ttl = self
		.services
		.config
		.authentication
		.oidc
		.browser_session_ttl
		.min(TTL_MAX);

	ttl.gt(&0).then_some(Duration::from_secs(ttl))
}
