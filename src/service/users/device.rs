use std::sync::Arc;

use futures::{Stream, StreamExt};
use ruma::{
	DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, UserId,
	api::client::device::Device, events::AnyToDeviceEvent, serde::Raw,
};
use serde_json::json;
use tuwunel_core::{
	Err, Result, at, implement,
	utils::{self, ReadyExt, stream::TryIgnore},
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Json, Map};

/// Adds a new device to a user.
#[implement(super::Service)]
pub async fn create_device(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	token: &str,
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
	self.set_access_token(user_id, device_id, token)
		.await
}

/// Removes a device from a user.
#[implement(super::Service)]
pub async fn remove_device(&self, user_id: &UserId, device_id: &DeviceId) {
	let userdeviceid = (user_id, device_id);

	// Remove tokens
	if let Ok(old_token) = self
		.db
		.userdeviceid_token
		.qry(&userdeviceid)
		.await
	{
		self.db.userdeviceid_token.del(userdeviceid);
		self.db.token_userdeviceid.remove(&old_token);
	}

	// Remove todevice events
	let prefix = (user_id, device_id, Interfix);
	self.db
		.todeviceid_events
		.keys_prefix_raw(&prefix)
		.ignore_err()
		.ready_for_each(|key| self.db.todeviceid_events.remove(key))
		.await;

	// TODO: Remove onetimekeys

	increment(&self.db.userid_devicelistversion, user_id.as_bytes());

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

/// Replaces the access token of one device.
#[implement(super::Service)]
pub async fn set_access_token(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	token: &str,
) -> Result {
	let key = (user_id, device_id);
	if self
		.db
		.userdeviceid_metadata
		.qry(&key)
		.await
		.is_err()
	{
		return Err!(Database(error!(
			?user_id,
			?device_id,
			"User does not exist or device has no metadata."
		)));
	}

	// Remove old token
	if let Ok(old_token) = self.db.userdeviceid_token.qry(&key).await {
		self.db.token_userdeviceid.remove(&old_token);
		// It will be removed from userdeviceid_token by the insert later
	}

	// Assign token to user device combination
	self.db.userdeviceid_token.put_raw(key, token);
	self.db.token_userdeviceid.raw_put(token, key);

	Ok(())
}

/// Find out which user an access token belongs to.
#[implement(super::Service)]
pub async fn find_from_access_token(&self, token: &str) -> Result<(OwnedUserId, OwnedDeviceId)> {
	self.db
		.token_userdeviceid
		.get(token)
		.await
		.deserialized()
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
