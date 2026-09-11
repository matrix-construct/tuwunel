use std::{
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	task::{Context, Wake, Waker},
};

use futures::pin_mut;
use ruma::{UserId, user_id};
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::Engine;

use crate::{Services, test_utils::fixture, users::PASSWORD_SENTINEL};

struct Sequence {
	engine: Arc<Engine>,
	sequence: AtomicU64,
}

#[tokio::test]
async fn disabled_user_creation_publishes_both_rows_before_notifying() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	for (user, origin) in [
		(user_id!("@disabled:localhost"), None),
		(user_id!("@remote:elsewhere.invalid"), Some("ldap")),
	] {
		assert_atomic_creation(services, user, origin).await?;
	}

	Ok(())
}

async fn assert_atomic_creation(
	services: &Services,
	user: &UserId,
	origin: Option<&str>,
) -> Result {
	let observed = Arc::new(Sequence {
		engine: services.db.engine.clone(),
		sequence: AtomicU64::new(0),
	});

	let waker = Waker::from(observed.clone());
	let origin_changed = services.db["userid_origin"].watch_raw_prefix_once(user);

	pin_mut!(origin_changed);

	assert!(
		origin_changed
			.as_mut()
			.poll(&mut Context::from_waker(&waker))
			.is_pending()
	);

	let before = services.db.engine.current_sequence();

	services.users.create(user, None, origin).await?;
	let after = services.db.engine.current_sequence();

	assert_eq!(after, before.saturating_add(2));
	assert_eq!(observed.sequence.load(Ordering::SeqCst), after);
	assert!(
		origin_changed
			.as_mut()
			.poll(&mut Context::from_waker(&waker))
			.is_ready()
	);

	assert_eq!(services.users.origin(user).await?, origin.unwrap_or("password"));
	assert!(services.users.exists(user).await);
	assert!(services.users.is_deactivated(user).await?);
	assert!(!services.users.has_password(user).await?);
	assert!(
		services.db["userid_password"]
			.get(user)
			.await?
			.is_empty()
	);

	Ok(())
}

#[tokio::test]
async fn sentinel_creation_keeps_its_active_external_identity() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let user = user_id!("@external:localhost");

	users
		.create(user, Some(PASSWORD_SENTINEL), Some("ldap"))
		.await?;

	assert_eq!(users.origin(user).await?, "ldap");
	assert_eq!(users.password_hash(user).await?, PASSWORD_SENTINEL);
	assert!(users.exists(user).await);
	assert!(!users.is_deactivated(user).await?);
	assert!(!users.has_password(user).await?);
	Ok(())
}

impl Wake for Sequence {
	fn wake(self: Arc<Self>) { self.wake_by_ref(); }

	fn wake_by_ref(self: &Arc<Self>) {
		self.sequence
			.store(self.engine.current_sequence(), Ordering::SeqCst);
	}
}
