use std::collections::{BTreeMap, HashMap};

use axum::extract::State;
use futures::{
	FutureExt, StreamExt,
	future::{
		Either::{Left, Right},
		join, join4,
	},
};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, DeviceId, OwnedDeviceId, OwnedUserId, ServerName,
	UserId,
	api::{
		client::{device::Device, keys::get_keys},
		federation,
	},
	encryption::{CrossSigningKey, DeviceKeys},
	serde::Raw,
};
use serde_json::{json, value::to_raw_value};
use tuwunel_core::{
	Result, debug_warn, implement,
	utils::{
		BoolExt, IterStream,
		future::TryExtExt,
		json,
		stream::{BroadbandExt, ReadyExt},
	},
};
use tuwunel_service::{Services, users::parse_master_key};

use super::FailureMap;
use crate::Ruma;

#[derive(Default)]
struct Keys {
	device_keys: DeviceKeyMap,
	master_keys: CrossSigningKeys,
	self_signing_keys: CrossSigningKeys,
	user_signing_keys: CrossSigningKeys,
	failures: FailureMap,
}

type DeviceLists = BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>;
type DeviceKeyMap = BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, Raw<DeviceKeys>>>;
type ServerDevices<'a> = HashMap<&'a ServerName, DeviceLists>;
type LocalDeviceUser<'a> = (&'a UserId, &'a Vec<OwnedDeviceId>);
type CrossSigningKeys = BTreeMap<OwnedUserId, Raw<CrossSigningKey>>;

/// # `POST /_matrix/client/r0/keys/query`
///
/// Get end-to-end encryption keys for the given users.
///
/// - Always fetches users from other servers over federation
/// - Gets master keys, self-signing keys, user signing keys and device keys.
/// - The master and self-signing keys contain signatures that the user is
///   allowed to see
pub(crate) async fn get_keys_route(
	State(services): State<crate::State>,
	body: Ruma<get_keys::v3::Request>,
) -> Result<get_keys::v3::Response> {
	let sender_user = body.sender_user();

	get_keys_helper(
		&services,
		Some(sender_user),
		&body.device_keys,
		|u| u == sender_user,
		true, // Always allow local users to see device names of other local users
	)
	.await
}

pub(crate) async fn get_keys_helper<F>(
	services: &Services,
	sender_user: Option<&UserId>,
	device_keys_input: &DeviceLists,
	allowed_signatures: F,
	include_display_names: bool,
) -> Result<get_keys::v3::Response>
where
	F: Fn(&UserId) -> bool + Send + Sync,
{
	let (local_users, remote_users): (Vec<LocalDeviceUser<'_>>, Vec<_>) = device_keys_input
		.iter()
		.map(|(uid, dids)| (uid.as_ref(), dids))
		.partition(|(user_id, _)| services.globals.user_is_local(user_id));

	let server: ServerDevices<'_> =
		remote_users
			.into_iter()
			.fold(HashMap::new(), |mut acc, (user_id, device_ids)| {
				acc.entry(user_id.server_name())
					.or_default()
					.insert(user_id.to_owned(), device_ids.clone());
				acc
			});

	let local = collect_local_keys(
		services,
		&local_users,
		sender_user,
		&allowed_signatures,
		include_display_names,
	);

	let federation = collect_federation_keys(services, server, sender_user, &allowed_signatures);

	let (local, federation) = join(local, federation).await;
	Ok(local.merge(federation).into_response())
}

async fn collect_local_keys<F>(
	services: &Services,
	users: &[LocalDeviceUser<'_>],
	sender_user: Option<&UserId>,
	allowed_signatures: &F,
	include_display_names: bool,
) -> Keys
where
	F: Fn(&UserId) -> bool + Send + Sync,
{
	users
		.iter()
		.copied()
		.stream()
		.broad_then(async |(user_id, device_ids)| {
			collect_local_user_keys(
				services,
				user_id,
				device_ids,
				sender_user,
				allowed_signatures,
				include_display_names,
			)
			.await
		})
		.ready_fold(Keys::default(), Keys::merge)
		.await
}

async fn collect_local_user_keys<F>(
	services: &Services,
	user_id: &UserId,
	device_ids: &[OwnedDeviceId],
	sender_user: Option<&UserId>,
	allowed_signatures: &F,
	include_display_names: bool,
) -> Keys
where
	F: Fn(&UserId) -> bool + Send + Sync,
{
	let device_keys =
		collect_local_device_keys(services, user_id, device_ids, include_display_names);

	let master_key = services
		.users
		.get_master_key(sender_user, user_id, allowed_signatures)
		.ok();

	let self_signing_key = services
		.users
		.get_self_signing_key(sender_user, user_id, allowed_signatures)
		.ok();

	let user_signing_key = (Some(user_id) == sender_user)
		.then_async(|| services.users.get_user_signing_key(user_id).ok())
		.map(Option::flatten);

	let (device_keys, master_key, self_signing_key, user_signing_key) =
		join4(device_keys, master_key, self_signing_key, user_signing_key).await;

	let owned = || user_id.to_owned();
	Keys {
		device_keys: BTreeMap::from([(owned(), device_keys)]),
		master_keys: master_key
			.map(|k| (owned(), k))
			.into_iter()
			.collect(),

		self_signing_keys: self_signing_key
			.map(|k| (owned(), k))
			.into_iter()
			.collect(),

		user_signing_keys: user_signing_key
			.map(|k| (owned(), k))
			.into_iter()
			.collect(),

		..Default::default()
	}
}

async fn collect_local_device_keys(
	services: &Services,
	user_id: &UserId,
	device_ids: &[OwnedDeviceId],
	include_display_names: bool,
) -> BTreeMap<OwnedDeviceId, Raw<DeviceKeys>> {
	let stream = if device_ids.is_empty() {
		Left(
			services
				.users
				.all_device_ids(user_id)
				.map(ToOwned::to_owned),
		)
	} else {
		Right(device_ids.iter().cloned().stream())
	};

	stream
		.broad_filter_map(async |device_id| {
			get_local_device_keys(services, user_id, &device_id, include_display_names)
				.await
				.map(|keys| (device_id, keys))
		})
		.collect()
		.await
}

async fn get_local_device_keys(
	services: &Services,
	user_id: &UserId,
	device_id: &DeviceId,
	include_display_names: bool,
) -> Option<Raw<DeviceKeys>> {
	let mut keys = services
		.users
		.get_device_keys(user_id, device_id)
		.await
		.ok()?;

	let metadata = services
		.users
		.get_device_metadata(user_id, device_id)
		.await
		.inspect_err(|e| debug_warn!(?user_id, ?device_id, "device metadata missing: {e}"))
		.ok()?;

	add_unsigned_device_display_name(&mut keys, metadata, include_display_names)
		.inspect_err(|e| debug_warn!(?user_id, ?device_id, "invalid device keys: {e}"))
		.ok()?;

	Some(keys)
}

async fn collect_federation_keys<F>(
	services: &Services,
	server: ServerDevices<'_>,
	sender_user: Option<&UserId>,
	allowed_signatures: &F,
) -> Keys
where
	F: Fn(&UserId) -> bool + Send + Sync,
{
	server
		.into_iter()
		.stream()
		.broad_then(async |(server, device_keys)| {
			let request = federation::keys::get_keys::v1::Request { device_keys };

			(server, services.federation.execute(server, request).await)
		})
		.broad_then(async |(server, response)| match response {
			| Ok(response) =>
				process_federation_response(services, sender_user, allowed_signatures, response)
					.await,
			| Err(e) => {
				debug_warn!(%server, "key federation request failed: {e}");
				Keys {
					failures: BTreeMap::from([(server.to_string(), json!({}))]),
					..Default::default()
				}
			},
		})
		.ready_fold(Keys::default(), Keys::merge)
		.await
}

async fn process_federation_response<F>(
	services: &Services,
	sender_user: Option<&UserId>,
	allowed_signatures: &F,
	response: federation::keys::get_keys::v1::Response,
) -> Keys
where
	F: Fn(&UserId) -> bool + Send + Sync,
{
	let federation::keys::get_keys::v1::Response {
		master_keys,
		self_signing_keys,
		device_keys,
	} = response;

	let master_keys = master_keys
		.into_iter()
		.stream()
		.broad_filter_map(async |(user, master_key)| {
			merge_remote_master_key(services, sender_user, allowed_signatures, &user, master_key)
				.await
				.inspect_err(|e| debug_warn!(?user, "skipping master key from federation: {e}"))
				.map(|raw| (user, raw))
				.ok()
		})
		.collect()
		.await;

	Keys {
		device_keys,
		master_keys,
		self_signing_keys,
		user_signing_keys: BTreeMap::new(),
		failures: BTreeMap::new(),
	}
}

/// Merges signatures from our cached copy of the user's master key (if any)
/// onto the remote-supplied master key, persists the merged copy to our
/// database, and returns the merged Raw value for the response.
async fn merge_remote_master_key<F>(
	services: &Services,
	sender_user: Option<&UserId>,
	allowed_signatures: &F,
	user: &UserId,
	master_key_raw: Raw<CrossSigningKey>,
) -> Result<Raw<CrossSigningKey>>
where
	F: Fn(&UserId) -> bool + Send + Sync,
{
	let (master_key_id, mut master_key) = parse_master_key(user, &master_key_raw)?;
	let our_raw = services
		.users
		.get_key(&master_key_id, sender_user, user, allowed_signatures)
		.await;

	if let Ok(our_raw) = our_raw
		&& let Ok((_, mut ours)) = parse_master_key(user, &our_raw)
	{
		master_key.signatures.append(&mut ours.signatures);
	}

	let raw = json::to_raw(&master_key)?;

	// Don't notify: a notification would trigger another key request resulting
	// in an endless loop.
	services
		.users
		.add_cross_signing_keys(user, &Some(raw.clone()), &None, &None, false)
		.await?;

	Ok(raw)
}

fn add_unsigned_device_display_name(
	keys: &mut Raw<DeviceKeys>,
	metadata: Device,
	include_display_names: bool,
) -> Result {
	let Some(display_name) = metadata.display_name else {
		return Ok(());
	};

	let mut object = keys.deserialize_as_unchecked::<CanonicalJsonObject>()?;

	if let CanonicalJsonValue::Object(unsigned) = object
		.entry("unsigned".into())
		.or_insert_with(|| CanonicalJsonObject::default().into())
	{
		let display_name = if include_display_names {
			CanonicalJsonValue::String(display_name.to_string())
		} else {
			CanonicalJsonValue::String(metadata.device_id.into())
		};

		unsigned.insert("device_display_name".into(), display_name);
	}

	*keys = Raw::from_json(to_raw_value(&object)?);

	Ok(())
}

#[implement(Keys)]
fn merge(mut self, other: Self) -> Self {
	self.failures.extend(other.failures);
	self.device_keys.extend(other.device_keys);
	self.master_keys.extend(other.master_keys);
	self.self_signing_keys
		.extend(other.self_signing_keys);
	self.user_signing_keys
		.extend(other.user_signing_keys);
	self
}

#[implement(Keys)]
fn into_response(self) -> get_keys::v3::Response {
	get_keys::v3::Response {
		failures: self.failures,
		device_keys: self.device_keys,
		master_keys: self.master_keys,
		self_signing_keys: self.self_signing_keys,
		user_signing_keys: self.user_signing_keys,
	}
}
