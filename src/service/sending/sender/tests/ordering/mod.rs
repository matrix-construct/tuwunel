use std::iter::once;

use futures::StreamExt;
use ruma::{
	ServerName, UserId,
	api::federation::transactions::edu::{Edu, SigningKeyUpdateContent},
	encryption::CrossSigningKey,
	room_id,
	serde::Raw,
	server_name, user_id,
};
use serde_json::{from_value, json};
use tuwunel_core::{Err, Result};

use super::{delivered, enqueue, fixture::fixture};
use crate::{
	sending::{
		Destination, SendingEvent,
		data::Keys,
		sender::{
			DEQUEUE_LIMIT, SendingFutures, TransactionStatuses, WakeQueue,
			select::{Selection, edu_buf},
		},
	},
	test_utils::Fixture,
};

#[tokio::test]
async fn queued_signing_keys_precede_fresh_selection() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let server = server_name!("remote.example");
	let dest = Destination::Federation(server.to_owned());
	let user = user_id!("@keys:localhost");
	let old = signing(user, "b2xk")?;
	let fresh = signing(user, "bmV3")?;

	for _ in 0..=DEQUEUE_LIMIT {
		enqueue(sending, &dest, old.clone());
	}

	let count = key_change(&fixture, server, user).await?;
	let before = sending.db.get_latest_educount(server).await;
	let mut futures = SendingFutures::new(); // handle_response out-param
	let mut statuses = TransactionStatuses::new(); // handle_response out-param
	let mut wakes = WakeQueue::new(); // handle_response out-param

	sending
		.handle_response(delivered(&dest, Keys::new()), &mut futures, &mut statuses, &mut wakes)
		.await;

	let (keys, active): (Keys, Vec<_>) = sending
		.db
		.active_requests_for(&dest)
		.unzip()
		.await;

	assert_eq!(active, vec![old.clone(); DEQUEUE_LIMIT]);
	assert_eq!(sending.db.queued_requests(&dest).count().await, 1);
	assert_eq!(sending.db.get_latest_educount(server).await, before);
	// Sends stay unpolled; the test inspects composition and simulates ACKs without network I/O.
	futures.clear();
	sending
		.handle_response(delivered(&dest, keys), &mut futures, &mut statuses, &mut wakes)
		.await;

	let active: Vec<_> = sending
		.db
		.active_requests_for(&dest)
		.map(|(_, event)| event)
		.collect()
		.await;

	assert_eq!(active, [old, fresh]);
	assert_eq!(sending.db.queued_requests(&dest).count().await, 0);
	assert_eq!(sending.db.get_latest_educount(server).await, count);

	Ok(())
}

#[tokio::test]
async fn flush_resumes_queued_backlog() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let dest = Destination::Federation(server_name!("remote.example").to_owned());
	let old = signing(user_id!("@keys:localhost"), "b2xk")?;

	for _ in 0..=DEQUEUE_LIMIT {
		enqueue(sending, &dest, old.clone());
	}

	let mut statuses = TransactionStatuses::new(); // select_events out-param
	let flush = [(Vec::new(), SendingEvent::Flush)].into();
	let Selection::Events(items) = sending
		.select_events(&dest, flush, &mut statuses)
		.await?
	else {
		return Err!("flush selects the queued backlog");
	};

	assert_eq!(items.len(), DEQUEUE_LIMIT);
	assert!(items.iter().all(|(_, event)| *event == old));
	assert_eq!(sending.db.queued_requests(&dest).count().await, 1);
	assert_eq!(
		sending
			.db
			.active_requests_for(&dest)
			.count()
			.await,
		DEQUEUE_LIMIT
	);

	Ok(())
}

#[tokio::test]
async fn success_acknowledges_only_carried_rows() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let server = server_name!("remote.example");
	let dest = Destination::Federation(server.to_owned());
	let user = user_id!("@keys:localhost");
	let old = enqueue(sending, &dest, signing(user, "b2xk")?);

	sending.db.mark_as_active(once(&old));
	key_change(&fixture, server, user).await?;

	let mut statuses = TransactionStatuses::new(); // select_events out-param
	let flush = [(Vec::new(), SendingEvent::Flush)].into();
	let Selection::Events(items) = sending
		.select_events(&dest, flush, &mut statuses)
		.await?
	else {
		return Err!("flush composes the fresh key update");
	};

	let active = sending
		.db
		.active_requests_for(&dest)
		.count()
		.await;

	assert_eq!((items.len(), active), (1, 2));

	let keys = items.into_iter().map(|(key, _)| key).collect();

	sending
		.handle_response(
			delivered(&dest, keys),
			&mut SendingFutures::new(),
			&mut statuses,
			&mut WakeQueue::new(),
		)
		.await;

	let active: Vec<_> = sending
		.db
		.active_requests_for(&dest)
		.collect()
		.await;

	assert_eq!(active, [old]);

	Ok(())
}

fn signing(user: &UserId, material: &str) -> Result<SendingEvent> {
	let key = key(user, material)?;
	let content = SigningKeyUpdateContent {
		user_id: user.to_owned(),
		master_key: Some(key),
		self_signing_key: None,
	};

	Ok(SendingEvent::Edu(edu_buf(&Edu::SigningKeyUpdate(content))))
}

/// Record a cross-signing key change that `server` must hear about.
///
/// Returns the change's count, the server's EDU watermark once it is sent.
async fn key_change(fixture: &Fixture, server: &ServerName, user: &UserId) -> Result<u64> {
	let room = room_id!("!keys:localhost");
	let key = key(user, "bmV3")?;

	fixture
		.services
		.users
		.add_cross_signing_keys(user, &Some(key), &None, &None, false)
		.await?;

	fixture.services.db["serverroomids"].put_raw((server, room), b"");

	let count = fixture.services.globals.next_count();

	// A cross-signing change (kind 3) carries no stream id or device.
	fixture.services.db["keychangeid_devicechange"].put(*count, (3_u8, 0_u64, ""));
	fixture.services.db["keychangeid_userid"].put_raw((room, *count), user.as_bytes());

	Ok(*count)
}

fn key(user: &UserId, material: &str) -> Result<Raw<CrossSigningKey>> {
	Ok(from_value(json!({
		"user_id": user,
		"usage": ["master"],
		"keys": {"ed25519:key": material}
	}))?)
}
