use std::{
	net::IpAddr,
	time::{Duration, SystemTime},
};

use futures::{FutureExt, Stream, StreamExt, future::join};
use ruma::{
	DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, UserId,
	api::client::device::Device, events::AnyToDeviceEvent, serde::Raw,
};
use serde_json::json;
use tuwunel_core::{
	Err, Error, Result, async_noinline, at, debug_warn, implement, trace,
	utils::{
		BoolExt, MutexMapGuard, OptionExt, ReadyExt,
		future::OptionFutureExt,
		random_string,
		stream::{IterStream, TryIgnore},
		string::to_small_string,
		time::{
			duration_since_epoch, timepoint_from_epoch, timepoint_from_now, timepoint_has_passed,
		},
	},
	warn,
};
use tuwunel_database::{Cbor, Deserialized, Ignore, Interfix, Json, Txn};

use super::DeviceListChange;

type DeviceLock = MutexMapGuard<(OwnedUserId, OwnedDeviceId), ()>;

/// generated device ID length
const DEVICE_ID_LENGTH: usize = 10;

/// Longest device ID a client may choose, in bytes.
///
/// Synapse refuses one longer than 512 characters at login. Other servers
/// receive the ID in every device list update.
pub const MAX_DEVICE_ID_LENGTH: usize = 512;

/// generated user access token length
pub const TOKEN_LENGTH: usize = 32;

/// Adds a new device to a user.
///
/// Without a device ID one is generated. A chosen ID is refused when longer
/// than `MAX_DEVICE_ID_LENGTH` bytes or when it matches one of the user's
/// cross-signing key IDs. The device is written and given its tokens under the
/// device lock.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, access_token, refresh_token))]
pub async fn create_device(
	&self,
	user_id: &UserId,
	device_id: Option<&DeviceId>,
	(access_token, expires_in): (Option<&str>, Option<Duration>),
	refresh_token: Option<&str>,
	initial_device_display_name: Option<&str>,
	client_ip: Option<IpAddr>,
) -> Result<OwnedDeviceId> {
	let device_id = resolve_device_id(device_id);

	if !self.exists(user_id).await {
		return Err!(Request(InvalidParam(error!(
			"Called create_device for non-existent user {user_id}"
		))));
	}

	self.check_device_id(user_id, Some(&device_id))
		.await?;

	let lock = self.lock_device(user_id, &device_id).await;

	let notify = true;

	self.put_device_metadata(user_id, notify, &Device {
		device_id: device_id.clone(),
		display_name: initial_device_display_name.map(Into::into),
		last_seen_ts: Some(MilliSecondsSinceUnixEpoch::now()),
		last_seen_ip: client_ip.map(to_small_string),
	})
	.await;

	if let Some(access_token) = access_token {
		self.issue_tokens(&lock, user_id, &device_id, (access_token, expires_in), refresh_token)
			.await?;
	}

	Ok(device_id)
}

fn resolve_device_id(device_id: Option<&DeviceId>) -> OwnedDeviceId {
	// Treat an empty device_id ("") as unspecified.
	device_id
		.filter(|device_id| !device_id.as_str().is_empty())
		.map(ToOwned::to_owned)
		.unwrap_or_else(|| OwnedDeviceId::from(random_string(DEVICE_ID_LENGTH)))
}

/// Refuses a device ID the user may not take.
///
/// An ID longer than `MAX_DEVICE_ID_LENGTH` bytes is refused. So is one
/// coinciding with one of the user's cross-signing key IDs, since both share
/// the device key row space. Without an ID nothing is refused.
#[implement(super::Service)]
pub async fn check_device_id(&self, user_id: &UserId, device_id: Option<&DeviceId>) -> Result {
	let Some(device_id) = device_id.map(DeviceId::as_str) else {
		return Ok(());
	};

	if device_id.len() > MAX_DEVICE_ID_LENGTH {
		return Err!(Request(InvalidParam("Device ID is too long.")));
	}

	if self
		.is_cross_signing_key_id(user_id, device_id)
		.await?
	{
		return Err!(Request(Forbidden("Device ID matches a cross-signing key ID.")));
	}

	Ok(())
}

/// Locks one device against concurrent creation, token issuance, metadata
/// updates and removal.
///
/// A refresh token is rotated by reading the one the device points at and
/// replacing it, so issuers run one after another to leave the device a single
/// refresh token. An issuer or metadata update that runs after removal finds
/// no device rather than writing for one already gone.
#[implement(super::Service)]
async fn lock_device(&self, user_id: &UserId, device_id: &DeviceId) -> DeviceLock {
	let key = (user_id.to_owned(), device_id.to_owned());

	self.updating_device.lock(&key).await
}

/// Removes a device from a user.
///
/// Everything keyed by the device goes with it, its identity keys included,
/// so a later session under the same ID starts with none.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn remove_device(&self, user_id: &UserId, device_id: &DeviceId) {
	let lock = self.lock_device(user_id, device_id).await;

	self.remove_device_locked(&lock, user_id, device_id)
		.await;
}

#[implement(super::Service)]
// query-depth firewall
#[async_noinline]
async fn remove_device_locked<'a>(
	&'a self,
	_lock: &'a DeviceLock,
	user_id: &'a UserId,
	device_id: &'a DeviceId,
) {
	self.remove_tokens(user_id, device_id).await;

	// Remove todevice events
	self.db
		.todeviceid_events
		.del_prefix(&(user_id, device_id, Interfix))
		.await;

	// Remove pushers
	self.services
		.pusher
		.get_device_pushkeys(user_id, device_id)
		.map(Vec::into_iter)
		.map(IterStream::stream)
		.flatten_stream()
		.for_each(async |pushkey| {
			self.services
				.pusher
				.delete_pusher(user_id, &pushkey)
				.await;
		})
		.await;

	// Removes the dehydrated device if the ID matches, otherwise no-op
	self.remove_dehydrated_device(user_id, Some(device_id))
		.await
		.ok();

	self.remove_one_time_keys(user_id, device_id)
		.await;

	// MSC2732: drop fallback keys for this device.
	self.db
		.userdeviceidalgorithm_fallback
		.del_prefix(&(user_id, device_id, Interfix))
		.await;

	// MSC3890: drop this device's local notification settings.
	let event_type = format!("org.matrix.msc3890.local_notification_settings.{device_id}").into();
	self.services
		.account_data
		.delete(None, user_id, event_type)
		.await
		.ok();

	let userdeviceid = (user_id, device_id);

	self.db.keyid_key.del(userdeviceid);
	self.db.userdeviceid_metadata.del(userdeviceid);
	self.db.oidcdevice_userdeviceid.del(userdeviceid);

	self.mark_device_key_update(user_id, DeviceListChange::Deleted(device_id))
		.await;
}

/// Returns an iterator over all device ids of this user.
#[implement(super::Service)]
pub fn all_device_ids<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = &DeviceId> + Send + 'a {
	let prefix = (user_id, Interfix);
	self.db
		.userdeviceid_metadata
		.keys_prefix(&prefix)
		.ignore_err()
		.map(|(_, device_id): (Ignore, &DeviceId)| device_id)
}

/// Find out which user an access or refresh token belongs to.
#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip(self, token))]
pub async fn find_from_token(
	&self,
	token: &str,
) -> Result<(OwnedUserId, OwnedDeviceId, Option<SystemTime>)> {
	self.db
		.token_userdeviceid
		.get(token)
		.await
		.deserialized()
		.and_then(|(user_id, device_id, expires_at): (_, _, Option<u64>)| {
			let expires_at = expires_at
				.map(Duration::from_secs)
				.map(timepoint_from_epoch)
				.transpose()?;

			Ok((user_id, device_id, expires_at))
		})
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn remove_tokens(&self, user_id: &UserId, device_id: &DeviceId) {
	let remove_access = self
		.remove_access_token(user_id, device_id)
		.map(Result::ok);

	let remove_refresh = self
		.remove_refresh_token(user_id, device_id)
		.map(Result::ok);

	join(remove_access, remove_refresh).await;
}

/// Replaces the access token of one device, and its refresh token when one is
/// given.
///
/// The tokens are issued under the device lock, and refused when the device no
/// longer exists.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, access_token, refresh_token))]
pub async fn set_access_token(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	access_token: &str,
	expires_in: Option<Duration>,
	refresh_token: Option<&str>,
) -> Result {
	let lock = self.lock_device(user_id, device_id).await;

	self.issue_tokens(&lock, user_id, device_id, (access_token, expires_in), refresh_token)
		.await
}

/// Exchanges a presented refresh token for a new access token of its device,
/// returning the refresh token for the client to keep.
///
/// Reuse is judged by `seen`, the caller's classification when the request
/// arrived: a token already spent outside the reuse grace window is refused,
/// and its device revoked when so configured. Otherwise the token is classified
/// again under the device lock, where a current one rotates and a spent one
/// inside the grace window keeps its successor, each once `gate`, the caller's
/// account check, passes. One that expired meanwhile is refused with the
/// configured logout, and one spent meanwhile by a concurrent exchange is
/// refused alone.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn rotate_refresh_token<Gate>(
	&self,
	seen: &RefreshToken,
	presented: &str,
	access: (&str, Option<Duration>),
	gate: Gate,
) -> Result<String>
where
	Gate: Future<Output = Result> + Send,
{
	let Some((user_id, device_id)) = seen.owner() else {
		return Err!(Request(Forbidden("Refresh token is unrecognized.")));
	};

	let lock = self.lock_device(user_id, device_id).await;

	if seen.reused() {
		return self
			.revoke_reused_refresh_token(&lock, user_id, device_id)
			.map(Err)
			.await;
	}

	let token = self
		.classify_refresh_token(presented)
		.map(|token| token.owned_by(user_id, device_id))
		.await;

	let exchangeable = token.exchangeable();
	let kept = match token {
		| RefreshToken::Unknown =>
			return Err!(Request(Forbidden("Refresh token is unrecognized."))),

		| RefreshToken::Current { .. } if !exchangeable =>
			return self
				.expire_refresh_token(&lock, user_id, device_id)
				.map(Err)
				.await,

		| RefreshToken::Replayed { .. } if !exchangeable =>
			return Err!(Request(Forbidden("Refresh token has already been used."))),

		| RefreshToken::Current { .. } => None,
		| RefreshToken::Replayed { current, .. } => Some(current),
	};

	gate.await?;

	let rotated = kept.is_none();
	let refresh_token = kept.unwrap_or_else(generate_refresh_token);

	let rotated = rotated.then_some(refresh_token.as_str());

	self.issue_tokens(&lock, user_id, device_id, access, rotated)
		.await?;

	Ok(refresh_token)
}

/// The user and device a recognized refresh token belongs to.
///
/// A spent token keeps naming its device, so a reuse can still be traced to
/// it, while an unrecognized token has no owner.
#[implement(RefreshToken)]
#[must_use]
pub fn owner(&self) -> Option<(&UserId, &DeviceId)> {
	match self {
		| Self::Current { user_id, device_id, .. }
		| Self::Replayed { user_id, device_id, .. } => Some((user_id, device_id)),
		| Self::Unknown => None,
	}
}

/// Whether the token was presented again after its rotation, outside the reuse
/// grace window.
#[implement(RefreshToken)]
#[must_use]
pub fn reused(&self) -> bool { matches!(self, Self::Replayed { grace: false, .. }) }

/// Narrows the classification to one device.
///
/// A token belonging to any other device is unknown to it.
#[implement(RefreshToken)]
fn owned_by(self, user_id: &UserId, device_id: &DeviceId) -> Self {
	let owned = self.owner() == Some((user_id, device_id));

	owned.then_some(self).unwrap_or(Self::Unknown)
}

/// Whether the token may be exchanged for new tokens as classified.
///
/// A current token qualifies until it expires, and a spent one only inside the
/// reuse grace window.
#[implement(RefreshToken)]
#[must_use]
pub fn exchangeable(&self) -> bool {
	match self {
		| Self::Current { expires_at, .. } => !expires_at.is_some_and(timepoint_has_passed),
		| Self::Replayed { grace, .. } => *grace,
		| Self::Unknown => false,
	}
}

/// Refuses an expired refresh token, retiring it first.
///
/// With hard logout the whole device is removed, otherwise only its refresh
/// token, leaving the session to log in again on the same device.
#[implement(super::Service)]
async fn expire_refresh_token(
	&self,
	lock: &DeviceLock,
	user_id: &UserId,
	device_id: &DeviceId,
) -> Error {
	let config = &self.services.server.config;
	let hard = config.refresh_token_hard_logout;

	hard.then_async(|| self.remove_device_locked(lock, user_id, device_id))
		.unwrap_or_else_async(async || {
			self.remove_refresh_token(user_id, device_id)
				.await
				.ok();
		})
		.await;

	Error::unknown_token(!hard, "Refresh token has expired.")
}

/// Refuses a refresh token presented again after its rotation.
///
/// Reuse outside the grace window suggests the token was copied, so the device
/// is removed when `refresh_token_reuse_revoke` is set.
#[implement(super::Service)]
async fn revoke_reused_refresh_token(
	&self,
	lock: &DeviceLock,
	user_id: &UserId,
	device_id: &DeviceId,
) -> Error {
	let config = &self.services.server.config;
	let revoke = config.refresh_token_reuse_revoke;

	if revoke {
		warn!(%user_id, %device_id, "Revoking a device whose refresh token was reused.");
		self.remove_device_locked(lock, user_id, device_id)
			.await;
	} else {
		debug_warn!(%user_id, %device_id, "Refresh token reused after rotation.");
	}

	Error::unknown_token(!revoke, "Refresh token has already been used.")
}

/// Issues an access token, and a refresh token when one is given, to one
/// device.
///
/// A device removed before its lock was taken gets none.
#[implement(super::Service)]
async fn issue_tokens(
	&self,
	lock: &DeviceLock,
	user_id: &UserId,
	device_id: &DeviceId,
	(access_token, expires_in): (&str, Option<Duration>),
	refresh_token: Option<&str>,
) -> Result {
	assert!(
		access_token.len() >= TOKEN_LENGTH,
		"Caller must supply an access_token >= {TOKEN_LENGTH} chars."
	);

	if !self.device_exists(user_id, device_id).await {
		return Err!(Request(Forbidden("Cannot issue tokens for a removed device.")));
	}

	if let Some(refresh_token) = refresh_token {
		self.set_refresh_token(lock, user_id, device_id, refresh_token)
			.await?;
	}

	let expires_at = expires_in
		.map(timepoint_from_now)
		.transpose()?
		.map(duration_since_epoch)
		.as_ref()
		.map(Duration::as_secs);

	let userdeviceid = (user_id, device_id);

	// Fold the prior pointer token into the index for pre-index upgrades.
	let previous = self
		.db
		.userdeviceid_token
		.qry(&userdeviceid)
		.await
		.deserialized::<String>()
		.ok();

	let mut txn = self.services.db.txn();

	if let Some(previous) = previous.as_deref() {
		let key = (user_id, device_id, previous);

		txn.put_raw(&self.db.userdeviceidtoken_index, key, []);
	}

	let key = (user_id, device_id, access_token);
	let value = (user_id, device_id, expires_at);

	txn.raw_put(&self.db.token_userdeviceid, access_token, value);
	txn.put_raw(&self.db.userdeviceidtoken_index, key, []);
	txn.put_raw(&self.db.userdeviceid_token, userdeviceid, access_token);

	txn.execute();

	Ok(())
}

/// Revoke every access token of one device, without deleting the device. Take
/// care to not leave dangling devices if using this method.
#[implement(super::Service)]
pub async fn remove_access_token(&self, user_id: &UserId, device_id: &DeviceId) -> Result {
	let prefix = (user_id, device_id, Interfix);
	self.db
		.userdeviceidtoken_index
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|(_, _, token): (Ignore, Ignore, &str)| {
			self.db.token_userdeviceid.remove(token);
			self.db
				.userdeviceidtoken_index
				.del((user_id, device_id, token));
		})
		.await;

	// Cover any pre-index token still recorded only in the legacy pointer.
	let token = self
		.db
		.userdeviceid_token
		.qry(&(user_id, device_id))
		.await
		.deserialized::<String>()
		.ok();

	let mut txn = self.services.db.txn();

	if let Some(token) = token.as_deref() {
		txn.del_raw(&self.db.token_userdeviceid, token);
	}

	txn.del(&self.db.userdeviceid_token, (user_id, device_id));

	txn.execute();

	Ok(())
}

/// Revoke a single access token by value, leaving the device and any other
/// tokens it holds intact.
#[implement(super::Service)]
pub async fn remove_access_token_value(&self, access_token: &str) {
	let owner = self
		.db
		.token_userdeviceid
		.get(access_token)
		.await
		.deserialized::<(OwnedUserId, OwnedDeviceId, Option<u64>)>()
		.ok();

	let mut txn = self.services.db.txn();

	if let Some((user_id, device_id, _)) = owner {
		let user_device_token = (&*user_id, &*device_id, access_token);

		txn.del(&self.db.userdeviceidtoken_index, user_device_token);
	}

	txn.del_raw(&self.db.token_userdeviceid, access_token);

	txn.execute();
}

#[implement(super::Service)]
pub fn generate_access_token(&self, expires: bool) -> (String, Option<Duration>) {
	let access_token = random_string(TOKEN_LENGTH);
	let expires_in = expires
		.then_some(self.services.server.config.access_token_ttl)
		.map(Duration::from_secs);

	(access_token, expires_in)
}

/// Replaces the refresh token of one device in a single write.
///
/// The outgoing token is retired in the same batch that stores its successor,
/// so a token presented meanwhile classifies as current or as spent, never as
/// unknown. Absolute expiry carries the prior deadline forward.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, _lock, refresh_token))]
async fn set_refresh_token(
	&self,
	_lock: &DeviceLock,
	user_id: &UserId,
	device_id: &DeviceId,
	refresh_token: &str,
) -> Result {
	debug_assert!(refresh_token.starts_with("refresh_"), "refresh_token missing prefix");

	let config = &self.services.server.config;
	let ttl = config.refresh_token_ttl;
	let idle_only = config.refresh_token_idle_only;
	let userdeviceid = (user_id, device_id);
	let outgoing = self
		.db
		.userdeviceid_refresh
		.qry(&userdeviceid)
		.await
		.ok();

	// Absolute mode carries the prior deadline forward instead of sliding it.
	let prior_expires_at = outgoing
		.as_deref()
		.filter(|_| ttl != 0 && !idle_only)
		.map_async(|outgoing| self.find_refresh_token_expires_at(outgoing))
		.await
		.flatten();

	let expires_at = match (ttl, prior_expires_at) {
		| (0, _) => None,
		| (_, Some(prior)) => Some(prior),
		| (ttl, None) => Some(timepoint_from_now(Duration::from_secs(ttl))?),
	};

	let expires_at_secs = expires_at
		.map(duration_since_epoch)
		.as_ref()
		.map(Duration::as_secs);

	let value = (user_id, device_id, expires_at_secs);
	let mut txn = self.services.db.txn();

	if let Some(outgoing) = outgoing.as_deref() {
		txn.del_raw(&self.db.token_userdeviceid, outgoing);
	}

	// Queued before the spent pointer below is rewritten; the batch applies in order.
	self.forget_spent_refresh_token(user_id, device_id, &mut txn)
		.await;

	txn.raw_put(&self.db.token_userdeviceid, refresh_token, value);
	txn.put_raw(&self.db.userdeviceid_refresh, userdeviceid, refresh_token);

	// Retain the outgoing token as the device's spent token, pointing at its
	// successor so a double-submit can be distinguished from a replay.
	if let Some(spent) = outgoing.as_deref() {
		let spent_at = duration_since_epoch(SystemTime::now()).as_secs();
		let value = (user_id, device_id, refresh_token, spent_at);

		txn.raw_put(&self.db.spentrefresh_userdeviceid, spent, value);
		txn.put_raw(&self.db.userdeviceid_spentrefresh, userdeviceid, spent);
	}

	txn.execute();

	Ok(())
}

/// Looks up the expiry stored alongside a refresh token.
///
/// Pre-rotation entries carry no expiry and return `None`.
#[implement(super::Service)]
async fn find_refresh_token_expires_at(&self, token: &[u8]) -> Option<SystemTime> {
	let (_, _, expires_at_secs): (Ignore, Ignore, Option<u64>) = self
		.db
		.token_userdeviceid
		.get(token)
		.await
		.deserialized()
		.ok()?;

	expires_at_secs
		.map(Duration::from_secs)
		.map(timepoint_from_epoch)?
		.ok()
}

/// Revoke the refresh token without deleting the device. Take care to not leave
/// dangling devices if using this method.
#[implement(super::Service)]
pub async fn remove_refresh_token(&self, user_id: &UserId, device_id: &DeviceId) -> Result {
	let userdeviceid = (user_id, device_id);
	let refresh_token = self
		.db
		.userdeviceid_refresh
		.qry(&userdeviceid)
		.await;

	let mut txn = self.services.db.txn();

	if let Ok(refresh_token) = refresh_token {
		txn.del_raw(&self.db.token_userdeviceid, &refresh_token);
	}

	txn.del(&self.db.userdeviceid_refresh, userdeviceid);

	self.forget_spent_refresh_token(user_id, device_id, &mut txn)
		.await;

	txn.execute();

	Ok(())
}

/// Drop the spent (previous-generation) refresh token retained for reuse
/// detection, if any.
#[implement(super::Service)]
async fn forget_spent_refresh_token(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	txn: &mut Txn,
) {
	let userdeviceid = (user_id, device_id);

	if let Ok(spent) = self
		.db
		.userdeviceid_spentrefresh
		.qry(&userdeviceid)
		.await
	{
		txn.del_raw(&self.db.spentrefresh_userdeviceid, &spent);
	}

	txn.del(&self.db.userdeviceid_spentrefresh, userdeviceid);
}

/// Classification of a refresh token presented for rotation at a token
/// endpoint.
pub enum RefreshToken {
	/// The device's current refresh token; rotate it.
	Current {
		user_id: OwnedUserId,
		device_id: OwnedDeviceId,
		expires_at: Option<SystemTime>,
	},

	/// A spent (already-rotated) token retained for one generation. `grace` is
	/// set when its successor is still current and it was spent within the
	/// configured window, marking a benign double-submit rather than a replay;
	/// `current` is the successor for which to re-issue an access token.
	Replayed {
		user_id: OwnedUserId,
		device_id: OwnedDeviceId,
		current: String,
		grace: bool,
	},

	/// Not a recognised refresh token.
	Unknown,
}

/// Classify a presented refresh token for the token-endpoint rotation path.
#[implement(super::Service)]
pub async fn classify_refresh_token(&self, presented: &str) -> RefreshToken {
	// The current refresh token resolves and matches the device's active
	// pointer (an access token resolves but will not match).
	if let Ok((user_id, device_id, expires_at)) = self.find_from_token(presented).await {
		let current: Option<String> = self
			.db
			.userdeviceid_refresh
			.qry(&(&user_id, &device_id))
			.await
			.deserialized()
			.ok();

		if current.as_deref() == Some(presented) {
			return RefreshToken::Current { user_id, device_id, expires_at };
		}
	}

	// Otherwise it may be the one retained spent token: a benign double-submit
	// inside the grace window, or a replay to be treated as a compromise.
	let Ok((user_id, device_id, successor, spent_at)) = self
		.db
		.spentrefresh_userdeviceid
		.get(presented)
		.await
		.deserialized::<(OwnedUserId, OwnedDeviceId, String, u64)>()
	else {
		return RefreshToken::Unknown;
	};

	let current: Option<String> = self
		.db
		.userdeviceid_refresh
		.qry(&(&user_id, &device_id))
		.await
		.deserialized()
		.ok();

	let grace_window = self
		.services
		.server
		.config
		.refresh_token_reuse_grace;
	let elapsed = duration_since_epoch(SystemTime::now())
		.as_secs()
		.saturating_sub(spent_at);

	let grace = grace_window != 0
		&& elapsed <= grace_window
		&& current.as_deref() == Some(successor.as_str());

	RefreshToken::Replayed {
		user_id,
		device_id,
		current: successor,
		grace,
	}
}

#[must_use]
pub fn generate_refresh_token() -> String { format!("refresh_{}", random_string(TOKEN_LENGTH)) }

#[implement(super::Service)]
pub fn add_to_device_event(
	&self,
	sender: &UserId,
	target_user_id: &UserId,
	target_device_id: &DeviceId,
	event_type: &str,
	content: &serde_json::Value,
) -> u64 {
	let count = self.services.globals.next_count();

	let key = (target_user_id, target_device_id, *count);
	self.db.todeviceid_events.put(
		key,
		Json(json!({
			"type": event_type,
			"sender": sender,
			"content": content,
		})),
	);

	trace!(
		%target_user_id,
		%target_device_id,
		count = *count,
		%event_type,
		%sender,
		"to_device write",
	);

	*count
}

#[implement(super::Service)]
pub fn get_to_device_events<'a>(
	&'a self,
	user_id: &'a UserId,
	device_id: &'a DeviceId,
	since: Option<u64>,
	to: Option<u64>,
) -> impl Stream<Item = (u64, Raw<AnyToDeviceEvent>)> + Send + 'a {
	type Key<'a> = (&'a UserId, &'a DeviceId, u64);

	let from = (user_id, device_id, since.map_or(0, |since| since.saturating_add(1)));

	self.db
		.todeviceid_events
		.stream_from(&from)
		.ignore_err()
		.ready_take_while(move |((user_id_, device_id_, count), _): &(Key<'_>, _)| {
			user_id == *user_id_ && device_id == *device_id_ && to.is_none_or(|to| *count <= to)
		})
		.map(|((_, _, count), event)| (count, event))
}

#[implement(super::Service)]
pub async fn remove_to_device_events<Until>(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	until: Until,
) where
	Until: Into<Option<u64>> + Send,
{
	type Key<'a> = (&'a UserId, &'a DeviceId, u64);

	let until = until.into().unwrap_or(u64::MAX);
	let from = (user_id, device_id, until);
	self.db
		.todeviceid_events
		.rev_keys_from(&from)
		.ignore_err()
		.ready_take_while(move |(user_id_, device_id_, _): &Key<'_>| {
			user_id == *user_id_ && device_id == *device_id_
		})
		.ready_for_each(|key: Key<'_>| {
			self.db.todeviceid_events.del(key);
		})
		.await;
}

/// Records when and from where a device was last seen.
///
/// An address is kept from an earlier sighting when none is given, and the
/// time defaults to now. The device list is not notified.
#[implement(super::Service)]
pub async fn update_device_last_seen(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	last_seen_ip: Option<IpAddr>,
	last_seen_ts: Option<MilliSecondsSinceUnixEpoch>,
) -> Result {
	self.update_device_metadata(user_id, device_id, |device| {
		let last_seen_ip = last_seen_ip
			.map(to_small_string)
			.or(device.last_seen_ip);

		let last_seen_ts = last_seen_ts
			.unwrap_or_else(MilliSecondsSinceUnixEpoch::now)
			.into();

		let device = Device { last_seen_ip, last_seen_ts, ..device };
		let notify = false;

		(device, notify)
	})
	.await
}

/// Renames an existing device, notifying its device list only when the name
/// changes.
///
/// The rename holds the device lock like any other metadata update.
#[implement(super::Service)]
pub async fn rename_device(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	display_name: &str,
) -> Result {
	self.update_device_metadata(user_id, device_id, |device| {
		let notify = device.display_name.as_deref() != Some(display_name);
		let device = Device {
			display_name: Some(display_name.into()),
			..device
		};

		(device, notify)
	})
	.await
}

/// Rewrites the metadata of an existing device, notifying its device list when
/// `update` says so.
///
/// The read and the write hold the device lock, so a device removed in between
/// is not written back, and two updates never lose one another's fields. A
/// missing device is an error and `update` does not run. `update` must keep the
/// device ID; changing it panics.
#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip(self, update))]
pub async fn update_device_metadata<Update>(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	update: Update,
) -> Result
where
	Update: FnOnce(Device) -> (Device, bool) + Send,
{
	let _lock = self.lock_device(user_id, device_id).await;

	let device = self
		.get_device_metadata(user_id, device_id)
		.await?;

	let (device, notify) = update(device);

	assert_eq!(&device.device_id, device_id, "update must keep the device ID");
	self.put_device_metadata(user_id, notify, &device)
		.await;

	Ok(())
}

/// Persists device metadata and optionally announces the changed device.
///
/// Silent updates leave the device-list version and notification rows unchanged.
#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip(self, device))]
async fn put_device_metadata(&self, user_id: &UserId, notify: bool, device: &Device) {
	let key = (user_id, &device.device_id);
	self.db
		.userdeviceid_metadata
		.put(key, Json(device));

	if notify {
		self.mark_device_key_update(user_id, DeviceListChange::Device(&device.device_id))
			.boxed() // Size firewall for device creation and rename callers.
			.await;
	}
}

/// Get device metadata.
#[implement(super::Service)]
pub async fn get_device_metadata(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
) -> Result<Device> {
	self.db
		.userdeviceid_metadata
		.qry(&(user_id, device_id))
		.await
		.deserialized()
		.inspect(|device: &Device| {
			debug_assert_eq!(&device.device_id, device_id, "device_id mismatch");
		})
}

#[implement(super::Service)]
pub async fn device_exists(&self, user_id: &UserId, device_id: &DeviceId) -> bool {
	self.db
		.userdeviceid_metadata
		.contains(&(user_id, device_id))
		.await
}

#[implement(super::Service)]
pub async fn is_oidc_device(&self, user_id: &UserId, device_id: &DeviceId) -> bool {
	self.db
		.oidcdevice_userdeviceid
		.contains(&(user_id, device_id))
		.await
}

/// Returns the IdP that originally authenticated this device, if known.
/// Returns `None` for devices predating the idp_id field or non-OIDC devices.
#[implement(super::Service)]
pub async fn get_oidc_device_idp(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
) -> Option<String> {
	self.db
		.oidcdevice_userdeviceid
		.qry(&(user_id, device_id))
		.await
		.deserialized::<Json<String>>()
		.ok()
		.map(|Json(idp)| idp)
		.filter(|idp| !idp.is_empty())
}

#[implement(super::Service)]
pub fn mark_oidc_device(&self, user_id: &UserId, device_id: &DeviceId, idp_id: &str) {
	self.db
		.oidcdevice_userdeviceid
		.put((user_id, device_id), Json(idp_id));
}

/// Allow cross-signing key replacement without UIAA for the next 10 minutes.
/// Returns the expiry timestamp in milliseconds.
#[expect(clippy::must_use_candidate)]
#[implement(super::Service)]
pub fn allow_cross_signing_replacement(&self, user_id: &UserId) -> SystemTime {
	let duration = Duration::from_mins(10);
	let expires = timepoint_from_now(duration).expect("failed to create timepoint from now");

	self.db
		.oidccskeybypass_userid
		.raw_put(user_id, Cbor(expires));

	expires
}

/// Check if the user is allowed to replace cross-signing keys without UIAA.
#[implement(super::Service)]
pub async fn can_replace_cross_signing_keys(&self, user_id: &UserId) -> bool {
	let Ok(expires): Result<SystemTime, _> = self
		.db
		.oidccskeybypass_userid
		.get(user_id)
		.await
		.deserialized::<Cbor<_>>()
		.map(at!(0))
	else {
		return false;
	};

	if !timepoint_has_passed(expires) {
		return true;
	}

	self.db.oidccskeybypass_userid.remove(user_id);
	false
}

#[implement(super::Service)]
pub async fn get_devicelist_version(&self, user_id: &UserId) -> Result<u64> {
	self.db
		.userid_devicelistversion
		.get(user_id)
		.await
		.deserialized()
}

#[implement(super::Service)]
pub fn all_devices_metadata<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = Device> + Send + 'a {
	let key = (user_id, Interfix);
	self.db
		.userdeviceid_metadata
		.stream_prefix(&key)
		.ignore_err()
		.map(|(_, val): (Ignore, Device)| val)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn absent_device_id_is_generated() {
		let device_id = resolve_device_id(None);

		assert_eq!(device_id.as_str().len(), DEVICE_ID_LENGTH);
	}

	#[test]
	fn empty_device_id_is_generated() {
		let device_id = resolve_device_id(Some("".into()));

		assert_eq!(device_id.as_str().len(), DEVICE_ID_LENGTH);
	}

	#[test]
	fn provided_device_id_is_preserved() {
		let device_id = resolve_device_id(Some("HELLOWORLD".into()));

		assert_eq!(device_id.as_str(), "HELLOWORLD");
	}
}
