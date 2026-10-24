use std::iter::once;

use futures::{StreamExt, future::join, pin_mut, poll};
use ruma::{
	DeviceId, OneTimeKeyAlgorithm, OneTimeKeyId, UserId, device_id, encryption::OneTimeKey,
	serde::Raw, user_id,
};
use serde_json::{from_value, json};
use tokio::sync::Notify;
use tuwunel_core::{Result, config::Figment};

use super::{PASSWORD_SENTINEL, Service};
use crate::test_utils::fixture;

const OLD_PASSWORD: &str = "old password";
const NEW_PASSWORD: &str = "new password";

#[tokio::test]
async fn deactivation_before_password_change_rejects_write() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let user_id = user_id!("@deactivation-first:localhost");

	users
		.create(user_id, Some(OLD_PASSWORD), None)
		.await?;

	let password_guard = users.changing_password.lock(user_id).await;
	let password_clear = users.set_password(user_id, None);
	let password_change = users.set_password_if_active(user_id, Some(NEW_PASSWORD));

	pin_mut!(password_clear);
	pin_mut!(password_change);
	assert!(poll!(password_clear.as_mut()).is_pending());
	assert!(poll!(password_change.as_mut()).is_pending());

	drop(password_guard);

	password_clear.await?;
	assert!(password_change.await.is_err());
	assert!(users.is_deactivated(user_id).await?);

	Ok(())
}

#[tokio::test]
async fn password_change_before_deactivation_leaves_disabled() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let user_id = user_id!("@password-first:localhost");

	users
		.create(user_id, Some(OLD_PASSWORD), None)
		.await?;

	let checked = Notify::new();
	let release = Notify::new();
	let after_check = async {
		checked.notify_one();
		release.notified().await;
	};

	let password_change =
		users.set_password_if_active_after(user_id, Some(NEW_PASSWORD), after_check);

	let deactivate = async {
		checked.notified().await;

		let password_clear = users.set_password(user_id, None);

		pin_mut!(password_clear);
		assert!(poll!(password_clear.as_mut()).is_pending());
		release.notify_one();

		password_clear.await
	};

	let (password_change, password_clear) = join(password_change, deactivate).await;

	password_change?;
	password_clear?;
	assert!(users.is_deactivated(user_id).await?);

	Ok(())
}

#[tokio::test]
async fn password_locks_are_independent_and_cancel_safe() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let user_a = user_id!("@password-lock-a:localhost");
	let user_b = user_id!("@password-lock-b:localhost");

	users
		.create(user_a, Some(OLD_PASSWORD), None)
		.await?;

	users
		.create(user_b, Some(OLD_PASSWORD), None)
		.await?;

	let password_guard = users.changing_password.lock(user_a).await;

	let mut queued = Box::pin(users.set_password_if_active(user_a, Some(NEW_PASSWORD)));

	assert!(poll!(queued.as_mut()).is_pending());
	drop(queued);

	users
		.set_password_if_active(user_b, Some(NEW_PASSWORD))
		.await?;

	drop(password_guard);

	users
		.set_password_if_active(user_a, Some(NEW_PASSWORD))
		.await?;

	Ok(())
}

#[tokio::test]
async fn rejected_password_change_releases_lock() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let user_id = user_id!("@password-origin:localhost");

	users
		.create(user_id, Some(PASSWORD_SENTINEL), Some("ldap"))
		.await?;

	assert!(
		users
			.set_password_if_active(user_id, Some(NEW_PASSWORD))
			.await
			.is_err()
	);

	users.set_password(user_id, None).await?;
	assert!(users.is_deactivated(user_id).await?);

	Ok(())
}

#[tokio::test]
async fn unused_fallback_keys_stay_with_their_device() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let user_id = user_id!("@fallback:localhost");
	let device_id = device_id!("FALLBACK");
	let longer_id = device_id!("FALLBACK2");

	create_devices(users, user_id, &[device_id, longer_id]).await?;

	let key_id = <&OneTimeKeyId>::try_from("signed_curve25519:AAAAAQ")?;
	let key: Raw<OneTimeKey> = from_value(json!("fallback key"))?;

	users
		.add_fallback_keys(user_id, longer_id, once((key_id, &key)))
		.await?;

	let unused = async |device_id| {
		users
			.unused_fallback_key_algorithms(user_id, device_id)
			.collect::<Vec<_>>()
			.await
	};

	assert_eq!(unused(device_id).await, []);
	assert_eq!(unused(longer_id).await, [OneTimeKeyAlgorithm::SignedCurve25519]);

	users
		.take_fallback_key(user_id, longer_id, &OneTimeKeyAlgorithm::SignedCurve25519)
		.await?;

	assert_eq!(unused(longer_id).await, []);

	Ok(())
}

async fn create_devices(users: &Service, user_id: &UserId, device_ids: &[&DeviceId]) -> Result {
	users.create(user_id, None, None).await?;

	for &device_id in device_ids {
		users
			.create_device(user_id, Some(device_id), (None, None), None, None, None)
			.await?;
	}

	Ok(())
}
