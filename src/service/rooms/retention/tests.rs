use std::{
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	task::{Context, Wake, Waker},
};

use futures::{TryStreamExt, pin_mut};
use ruma::{CanonicalJsonObject, EventId, event_id, room_id};
use serde_json::{from_value, json};
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::Engine;

use crate::test_utils::fixture;

struct Sequence {
	engine: Arc<Engine>,
	observed: AtomicU64,
}

#[tokio::test]
async fn original_and_expiry_are_visible_before_notification() -> Result {
	let config = Figment::new().merge(("save_unredacted_events", true));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let event = event_id!("$original:localhost");
	let room = room_id!("!retention:localhost");
	let original: CanonicalJsonObject = from_value(json!({"content": {"body": "original"}}))?;
	let lock = services.state.mutex.lock(room).await;
	let observed = Arc::new(Sequence {
		engine: services.db.engine.clone(),
		observed: AtomicU64::new(0),
	});

	let waker = Waker::from(observed.clone());
	let notified = services.db["eventid_originalpdu"].watch_raw_prefix(event);

	pin_mut!(notified);

	assert!(
		notified
			.as_mut()
			.poll(&mut Context::from_waker(&waker))
			.is_pending()
	);

	services
		.retention
		.save_original_pdu(event, &original, &lock)
		.await;

	assert!(
		notified
			.as_mut()
			.poll(&mut Context::from_waker(&waker))
			.is_ready()
	);

	assert_eq!(observed.observed.load(Ordering::SeqCst), services.db.engine.current_sequence());
	let retained = services
		.retention
		.get_original_pdu_json(event)
		.await?;

	assert_eq!(retained, original);

	let expiry: Vec<_> = services.db["timeredacted_eventid"]
		.keys()
		.map_ok(|(_, event): (u64, &EventId)| event.to_owned())
		.try_collect()
		.await?;

	assert_eq!(expiry, [event]);
	let before = services.db.engine.current_sequence();
	let replacement = from_value(json!({"content": {"body": "later"}}))?;

	services
		.retention
		.save_original_pdu(event, &replacement, &lock)
		.await;

	assert_eq!(services.db.engine.current_sequence(), before);
	let retained = services
		.retention
		.get_original_pdu_json(event)
		.await?;

	assert_eq!(retained, original);

	Ok(())
}

#[tokio::test]
async fn disabled_retention_writes_neither_row() -> Result {
	let config = Figment::new().merge(("save_unredacted_events", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!retention:localhost");
	let lock = services.state.mutex.lock(room).await;
	let before = services.db.engine.current_sequence();

	services
		.retention
		.save_original_pdu(event_id!("$disabled:localhost"), &CanonicalJsonObject::new(), &lock)
		.await;

	assert_eq!(services.db.engine.current_sequence(), before);
	assert_eq!(services.db["eventid_originalpdu"].count().await, 0);
	assert_eq!(services.db["timeredacted_eventid"].count().await, 0);

	Ok(())
}

impl Wake for Sequence {
	fn wake(self: Arc<Self>) {
		self.observed
			.store(self.engine.current_sequence(), Ordering::SeqCst);
	}
}
