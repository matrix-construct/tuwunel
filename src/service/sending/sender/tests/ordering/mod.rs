use futures::StreamExt;
use ruma::{
	UserId,
	api::federation::transactions::edu::{Edu, SigningKeyUpdateContent},
	encryption::CrossSigningKey,
	room_id,
	serde::Raw,
	server_name, user_id,
};
use serde_json::{from_value, json};
use tuwunel_core::Result;

use super::{enqueue, fixture::fixture};
use crate::sending::{
	Destination, SendingEvent,
	sender::{DEQUEUE_LIMIT, SendingFutures, TransactionStatuses, select::edu_buf},
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
	let room = room_id!("!keys:localhost");
	let old = signing(user, "b2xk")?;
	let fresh = signing(user, "bmV3")?;

	for _ in 0..=DEQUEUE_LIMIT {
		enqueue(sending, &dest, old.clone());
	}

	let key = key(user, "bmV3")?;

	fixture
		.services
		.users
		.add_cross_signing_keys(user, &Some(key), &None, &None, false)
		.await?;

	fixture.services.db["serverroomids"].put_raw((server, room), b"");

	let count = {
		let count = fixture.services.globals.next_count();

		fixture.services.db["keychangeid_devicechange"].put(*count, (3_u8, 0_u64, ""));
		fixture.services.db["keychangeid_userid"].put_raw((room, *count), user.as_bytes());
		*count
	};

	let before = sending.db.get_latest_educount(server).await;
	let mut futures = SendingFutures::new(); // handle_response_ok out-param
	let mut statuses = TransactionStatuses::new(); // handle_response_ok out-param

	sending
		.handle_response_ok(dest.clone(), &mut futures, &mut statuses)
		.await;

	let active: Vec<_> = sending
		.db
		.active_requests_for(&dest)
		.map(|(_, event)| event)
		.collect()
		.await;

	assert_eq!(active, vec![old.clone(); DEQUEUE_LIMIT]);
	assert_eq!(sending.db.queued_requests(&dest).count().await, 1);
	assert_eq!(sending.db.get_latest_educount(server).await, before);
	// Sends stay unpolled; the test inspects composition and simulates ACKs without network I/O.
	futures.clear();
	sending
		.handle_response_ok(dest.clone(), &mut futures, &mut statuses)
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
	let events = sending
		.select_events(&dest, flush, &mut statuses)
		.await?;

	assert_eq!(events, Some(vec![old; DEQUEUE_LIMIT]));
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

fn signing(user: &UserId, material: &str) -> Result<SendingEvent> {
	let key = key(user, material)?;
	let content = SigningKeyUpdateContent {
		user_id: user.to_owned(),
		master_key: Some(key),
		self_signing_key: None,
	};

	Ok(SendingEvent::Edu(edu_buf(&Edu::SigningKeyUpdate(content))))
}

fn key(user: &UserId, material: &str) -> Result<Raw<CrossSigningKey>> {
	Ok(from_value(json!({
		"user_id": user,
		"usage": ["master"],
		"keys": {"ed25519:key": material}
	}))?)
}
