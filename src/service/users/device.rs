use std::{
	sync::Arc,
	time::{Duration, SystemTime},
};

use futures::{FutureExt, Stream, StreamExt, future::join};
use ruma::{
	DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, UserId,
	api::client::device::Device, events::AnyToDeviceEvent, serde::Raw,
};
use serde_json::json;
use tuwunel_core::{
	Err, Result, at, implement,
	utils::{
		self, ReadyExt,
		stream::{IterStream, TryIgnore},
		time::{duration_since_epoch, timepoint_from_epoch, timepoint_from_now},
	},
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Json, Map};

/// generated user access token length
pub const TOKEN_LENGTH: usize = 32;

/// Adds a new device to a user.
#[implement(super::Service)]
#[tracing::instrument(level = "info", skip(self, access_token))]
pub async fn create_device(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	(access_token, expires_in): (Option<&str>, Option<Duration>),
	refresh_token: Option<&str>,
	initial_device_display_name: Option<String>,
	client_ip: Option<String>,
) -> Result {
	if !self.exists(user_id).await {
		return Err!(Request(InvalidParam(error!(
			"Called create_device for non-existent user {user_id}"
		))));
	}

	let key = (user_id, device_id);
	let val = Device {
		device_id: device_id.into(),
		display_name: initial_device_display_name,
		last_seen_ip: client_ip,
		last_seen_ts: Some(MilliSecondsSinceUnixEpoch::now()),
	};

	increment(&self.db.userid_devicelistversion, user_id.as_bytes());
	self.db.userdeviceid_metadata.put(key, Json(val));

	if let Some(access_token) = access_token {
		self.set_access_token(user_id, device_id, access_token, expires_in, refresh_token)
			.await?;
	}

	Ok(())
}

/// Removes a device from a user.
#[implement(super::Service)]
#[tracing::instrument(level = "info", skip(self))]
pub async fn remove_device(&self, user_id: &UserId, device_id: &DeviceId) {
	// Remove access tokens
	self.remove_tokens(user_id, device_id).await;

	// Remove todevice events
	let prefix = (user_id, device_id, Interfix);
	self.db
		.todeviceid_events
		.keys_prefix_raw(&prefix)
		.ignore_err()
		.ready_for_each(|key| self.db.todeviceid_events.remove(key))
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

	// TODO: Remove onetimekeys

	increment(&self.db.userid_devicelistversion, user_id.as_bytes());

	let userdeviceid = (user_id, device_id);
	self.db.userdeviceid_metadata.del(userdeviceid);
	self.mark_device_key_update(user_id).await;
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

/// Replaces the access token of one device.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn set_access_token(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	access_token: &str,
	expires_in: Option<Duration>,
	refresh_token: Option<&str>,
) -> Result {
	assert!(
		access_token.len() >= TOKEN_LENGTH,
		"Caller must supply an access_token >= {TOKEN_LENGTH} chars."
	);

	if let Some(refresh_token) = refresh_token {
		self.set_refresh_token(user_id, device_id, refresh_token)
			.await?;
	}

	// Remove old token.
	self.remove_access_token(user_id, device_id)
		.await
		.ok();

	let expires_at = expires_in
		.map(timepoint_from_now)
		.transpose()?
		.map(duration_since_epoch)
		.as_ref()
		.map(Duration::as_secs);

	let userdeviceid = (user_id, device_id);
	let value = (user_id, device_id, expires_at);
	self.db
		.token_userdeviceid
		.raw_put(access_token, value);
	self.db
		.userdeviceid_token
		.put_raw(userdeviceid, access_token);

	Ok(())
}

/// Revoke the access token without deleting the device. Take care to not leave
/// dangling devices if using this method.
#[implement(super::Service)]
pub async fn remove_access_token(&self, user_id: &UserId, device_id: &DeviceId) -> Result {
	let userdeviceid = (user_id, device_id);
	let access_token = self
		.db
		.userdeviceid_token
		.qry(&userdeviceid)
		.await?;

	self.db.userdeviceid_token.del(userdeviceid);
	self.db.token_userdeviceid.remove(&access_token);

	Ok(())
}

#[implement(super::Service)]
pub async fn get_access_token(&self, user_id: &UserId, device_id: &DeviceId) -> Result<String> {
	let key = (user_id, device_id);
	self.db
		.userdeviceid_token
		.qry(&key)
		.await
		.deserialized()
}

#[implement(super::Service)]
pub fn generate_access_token(&self, expires: bool) -> (String, Option<Duration>) {
	let access_token = utils::random_string(TOKEN_LENGTH);
	let expires_in = expires
		.then_some(self.services.server.config.access_token_ttl)
		.map(Duration::from_secs);

	(access_token, expires_in)
}

/// Replaces the refresh token of one device.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn set_refresh_token(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	refresh_token: &str,
) -> Result {
	debug_assert!(refresh_token.starts_with("refresh_"), "refresh_token missing prefix");

	// Remove old token
	self.remove_refresh_token(user_id, device_id)
		.await
		.ok();

	let userdeviceid = (user_id, device_id);
	self.db
		.token_userdeviceid
		.raw_put(refresh_token, userdeviceid);
	self.db
		.userdeviceid_refresh
		.put_raw(userdeviceid, refresh_token);

	Ok(())
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
		.await?;

	self.db.userdeviceid_refresh.del(userdeviceid);
	self.db.token_userdeviceid.remove(&refresh_token);

	Ok(())
}

#[implement(super::Service)]
pub async fn get_refresh_token(&self, user_id: &UserId, device_id: &DeviceId) -> Result<String> {
	let key = (user_id, device_id);
	self.db
		.userdeviceid_refresh
		.qry(&key)
		.await
		.deserialized()
}

#[must_use]
pub fn generate_refresh_token() -> String {
	format!("refresh_{}", utils::random_string(TOKEN_LENGTH))
}

#[implement(super::Service)]
pub async fn add_to_device_event(
	&self,
	sender: &UserId,
	target_user_id: &UserId,
	target_device_id: &DeviceId,
	event_type: &str,
	content: serde_json::Value,
) {
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
}

#[implement(super::Service)]
pub fn get_to_device_events<'a>(
	&'a self,
	user_id: &'a UserId,
	device_id: &'a DeviceId,
	since: Option<u64>,
	to: Option<u64>,
) -> impl Stream<Item = Raw<AnyToDeviceEvent>> + Send + 'a {
	type Key<'a> = (&'a UserId, &'a DeviceId, u64);

	let from = (user_id, device_id, since.map_or(0, |since| since.saturating_add(1)));

	self.db
		.todeviceid_events
		.stream_from(&from)
		.ignore_err()
		.ready_take_while(move |((user_id_, device_id_, count), _): &(Key<'_>, _)| {
			user_id == *user_id_ && device_id == *device_id_ && to.is_none_or(|to| *count <= to)
		})
		.map(at!(1))
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

#[implement(super::Service)]
pub async fn update_device_metadata(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	device: &Device,
) -> Result {
	increment(&self.db.userid_devicelistversion, user_id.as_bytes());

	let key = (user_id, device_id);
	self.db
		.userdeviceid_metadata
		.put(key, Json(device));

	Ok(())
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
}

#[implement(super::Service)]
pub async fn device_exists(&self, user_id: &UserId, device_id: &DeviceId) -> bool {
	self.db
		.userdeviceid_metadata
		.contains(&(user_id, device_id))
		.await
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

//TODO: this is an ABA
fn increment(db: &Arc<Map>, key: &[u8]) {
	let old = db.get_blocking(key);
	let new = utils::increment(old.ok().as_deref());
	db.insert(key, new);
}
