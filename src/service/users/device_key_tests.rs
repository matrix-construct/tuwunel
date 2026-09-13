use std::time::Duration;

use futures::{future::join, poll};
use ruma::{
	api::client::{dehydrated_device::put_dehydrated_device::unstable::Request, device::Device},
	device_id,
	encryption::{CrossSigningKey, DeviceKeys},
	serde::{Base64, Raw, base64::Standard},
	signatures::{Ed25519KeyPair, KeyPair, to_canonical_json_string_for_signing},
	user_id,
};
use serde_json::json;
use tokio::{task::unconstrained, time::timeout};
use tuwunel_core::{Result, config::Figment, utils::to_canonical_object};
use tuwunel_database::{Json, serialize_key};

use crate::{test_utils::fixture, users::PASSWORD_SENTINEL};

#[tokio::test]
async fn deletion_orders_identity_uploads_and_signatures() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};
	let services = &fixture.services;
	let users = &services.users;
	let user = user_id!("@keycleanup:localhost");
	let device = device_id!("DELETED");
	let other = device_id!("OTHER");
	let other_user = user_id!("@other:localhost");
	let signer = Ed25519KeyPair::from_der(&Ed25519KeyPair::generate(), "SIGNER".into())
		.expect("generated signer key should parse");
	let public_key = Base64::<Standard, _>::new(signer.public_key()).encode();
	let signing_id = format!("ed25519:{public_key}");
	let signing_key = json!({
		"user_id": user,
		"usage": ["self_signing"],
		"keys": {&signing_id: &public_key},
	});
	services.db["keyid_key"].put((user, &public_key), Json(&signing_key));
	services.db["userid_selfsigningkeyid"].insert(user, serialize_key((user, &public_key))?);
	let value = json!({
		"user_id": user,
		"device_id": device,
		"algorithms": ["m.olm.v1.curve25519-aes-sha2"],
		"keys": {"ed25519:DELETED": &public_key},
		"signatures": {},
	});
	let keys: Raw<DeviceKeys> = serde_json::from_value(value.clone())?;
	let canonical = to_canonical_json_string_for_signing(&to_canonical_object(&value)?)?;
	let signature = signer.sign(canonical.as_bytes()).base64();

	for signing in [false, true] {
		for deletion_first in [false, true] {
			users.put_device_metadata(user, false, &Device::new(device.to_owned()));
			services.db["keyid_key"].put((user, device), Json(&keys));
			let guard = users.key_update_mutex.lock(user).await;
			let write = async {
				if signing {
					users
						.sign_key(
							user,
							device.as_str(),
							[(signing_id.clone(), signature.clone())].into(),
							user,
						)
						.await
				} else {
					users.add_device_keys(user, device, &keys).await
				}
			};
			let mut write = Box::pin(unconstrained(write));
			let mut delete = Box::pin(unconstrained(users.remove_device(user, device)));

			// Poll while holding the lock to establish both orders without sleeps.
			if deletion_first {
				assert!(poll!(delete.as_mut()).is_pending(), "deletion bypassed the writer lock");
				assert!(poll!(write.as_mut()).is_pending(), "identity writer bypassed deletion");
			} else {
				assert!(poll!(write.as_mut()).is_pending(), "identity writer bypassed the lock");
				assert!(poll!(delete.as_mut()).is_pending(), "deletion bypassed the writer lock");
			}
			assert!(users.device_exists(user, device).await);
			assert_eq!(
				users
					.get_device_keys(user, device)
					.await?
					.json()
					.get(),
				keys.json().get()
			);

			// Another user remains independent of the queued operations.
			users.put_device_metadata(other_user, false, &Device::new(other.to_owned()));
			let mut other_keys = value.clone();
			other_keys["device_id"] = json!(other);
			other_keys["user_id"] = json!(other_user);
			let other_keys: Raw<DeviceKeys> = serde_json::from_value(other_keys)?;
			timeout(
				Duration::from_secs(5),
				users.add_device_keys(other_user, other, &other_keys),
			)
			.await
			.expect("another user's identity upload was blocked")?;

			drop(guard);
			let ((), written) = timeout(Duration::from_secs(5), join(delete, write))
				.await
				.expect("identity writer and deletion did not finish");
			if deletion_first {
				assert!(
					written.is_err_and(|error| error.is_not_found()),
					"writer restored a deleted device: signing={signing}"
				);
			} else {
				written?;
			}
			assert!(!users.device_exists(user, device).await);
			assert!(
				users
					.get_device_keys(user, device)
					.await
					.is_err_and(|error| error.is_not_found())
			);
			users
				.get_device_keys(other_user, other)
				.await
				.expect("another user's identity must remain");
			services.db["keyid_key"]
				.qry(&(user, &public_key))
				.await
				.expect("the self-signing key must remain");
		}
	}

	Ok(())
}

#[tokio::test]
async fn deletion_preserves_colliding_cross_signing_keys() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};
	let services = &fixture.services;
	let users = &services.users;
	let user = user_id!("@collision:localhost");

	for (index, (seed, usage)) in [(0_u8, "master"), (1, "self_signing"), (2, "user_signing")]
		.into_iter()
		.enumerate()
	{
		let public_key = Base64::<Standard>::new(vec![seed; 32]).encode();
		let device: &ruma::DeviceId = public_key.as_str().into();
		let value = json!({
			"user_id": user, "usage": [usage],
			"keys": {format!("ed25519:{public_key}"): &public_key},
		});
		let raw: Raw<CrossSigningKey> = serde_json::from_value(value.clone())?;
		let mut keys = [None, None, None];
		keys[index] = Some(raw);
		users
			.add_cross_signing_keys(user, &keys[0], &keys[1], &keys[2], false)
			.await?;
		users.put_device_metadata(user, false, &Device::new(device.to_owned()));
		users.remove_device(user, device).await;
		assert!(!users.device_exists(user, device).await);
		let stored = services.db["keyid_key"]
			.qry(&(user, device))
			.await?;
		assert_eq!(serde_json::from_slice::<serde_json::Value>(&stored)?, value);
	}

	Ok(())
}

#[tokio::test]
async fn dehydrated_device_replacement_cleans_identity() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};
	let users = &fixture.services.users;
	let user = user_id!("@dehydrated:localhost");
	users
		.create(user, Some(PASSWORD_SENTINEL), None)
		.await?;
	let mut previous: Option<ruma::OwnedDeviceId> = None;

	for (id, seed) in [("ASLEEP", 1_u8), ("ASLEEP", 2), ("REPLACED", 3)] {
		let device: &ruma::DeviceId = id.into();
		let public_key = Base64::<Standard>::new(vec![seed; 32]).encode();
		let value = json!({
			"user_id": user, "device_id": device,
			"algorithms": ["m.olm.v1.curve25519-aes-sha2"],
			"keys": {format!("ed25519:{device}"): public_key},
			"signatures": {},
		});
		let keys: Raw<DeviceKeys> = serde_json::from_value(value.clone())?;
		let data = serde_json::from_value(json!({
			"algorithm": "org.matrix.msc3814.v1.olm", "device_pickle": "encrypted-pickle",
		}))?;
		let request = Request::new(device.to_owned(), data, keys);
		timeout(Duration::from_secs(5), users.set_dehydrated_device(user, request))
			.await
			.expect("dehydrated replacement did not finish")?;
		assert_eq!(users.get_dehydrated_device_id(user).await?, device);
		let stored = users.get_device_keys(user, device).await?;
		assert_eq!(serde_json::from_str::<serde_json::Value>(stored.json().get())?, value);
		if let Some(previous) = previous.filter(|previous| previous != device) {
			assert!(
				users
					.get_device_keys(user, &previous)
					.await
					.is_err_and(|error| error.is_not_found())
			);
		}
		previous = Some(device.to_owned());
	}

	let device = previous.expect("the fixture created a dehydrated device");
	users.remove_device(user, &device).await;
	assert!(
		users
			.get_dehydrated_device_id(user)
			.await
			.is_err_and(|error| error.is_not_found())
	);
	assert!(
		users
			.get_device_keys(user, &device)
			.await
			.is_err_and(|error| error.is_not_found())
	);
	Ok(())
}

#[tokio::test]
async fn deletion_orders_colliding_cross_signing_updates() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};
	let services = &fixture.services;
	let users = &services.users;
	let user = user_id!("@crosssigning:localhost");
	let public_key = Base64::<Standard>::new(vec![1; 32]).encode();
	let device: &ruma::DeviceId = public_key.as_str().into();
	let value = json!({
		"user_id": user, "usage": ["master"],
		"keys": {format!("ed25519:{public_key}"): &public_key},
	});
	let keys: Raw<CrossSigningKey> = serde_json::from_value(value.clone())?;
	let keys = Some(keys);
	let absent = None;

	for deletion_first in [false, true] {
		services.db["userid_masterkeyid"].remove(user);
		services.db["keyid_key"].del((user, device));
		users.put_device_metadata(user, false, &Device::new(device.to_owned()));
		let guard = users.key_update_mutex.lock(user).await;
		let mut write = Box::pin(unconstrained(
			users.add_cross_signing_keys(user, &keys, &absent, &absent, false),
		));
		let mut delete = Box::pin(unconstrained(users.remove_device(user, device)));
		if deletion_first {
			assert!(poll!(delete.as_mut()).is_pending());
			assert!(poll!(write.as_mut()).is_pending());
		} else {
			assert!(poll!(write.as_mut()).is_pending());
			assert!(poll!(delete.as_mut()).is_pending());
		}
		assert!(
			services.db["keyid_key"]
				.qry(&(user, device))
				.await
				.is_err_and(|error| error.is_not_found())
		);
		drop(guard);
		let ((), written) = timeout(Duration::from_secs(5), join(delete, write))
			.await
			.expect("cross-signing writer and deletion did not finish");
		written?;
		assert!(!users.device_exists(user, device).await);
		let stored = users
			.get_master_key(None, user, &|_| true)
			.await?;
		assert_eq!(serde_json::from_str::<serde_json::Value>(stored.json().get())?, value);
	}
	Ok(())
}
