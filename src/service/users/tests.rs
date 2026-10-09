use futures::{future::join, pin_mut, poll};
use ruma::user_id;
use tokio::sync::Notify;
use tuwunel_core::{Result, config::Figment};

use super::PASSWORD_SENTINEL;
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
