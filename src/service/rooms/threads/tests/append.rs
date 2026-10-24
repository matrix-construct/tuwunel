use std::{pin::pin, sync::Arc, task::Waker};

use ruma::{EventId, RoomId, UserId, event_id, room_id, user_id};
use serde_json::{Value, json};
use tuwunel_core::{Result, config::Figment, utils::result::NotFound};
use tuwunel_database::Json;
use tuwunel_matrix::pdu::{PduCount, PduEvent, PduId, RawPduId};

use self::observation::Observation;
use crate::{
	Services,
	test_utils::{fixture, pdu_id, poll_once},
};

mod observation;

#[tokio::test]
async fn participant_notification_observes_activity_and_bundle() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!thread:localhost");
	let root = pdu(room_id, event_id!("$root:localhost"), user_id!("@root:localhost"));
	let reply = pdu(room_id, event_id!("$reply:localhost"), user_id!("@reply:localhost"));
	let root_id = pdu_id(1);
	let reply_id = pdu_id(2);
	let _guard = services.state.mutex.lock(room_id).await;

	store(services, &root_id, &root);

	let observation = Arc::new(Observation::new(services, root_id, reply_id));
	let waker = Waker::from(observation.clone());
	let mut watcher = pin!(services.db["threadid_userids"].watch_raw_prefix_once(root_id)); // Future::poll requires mutable access.

	assert!(poll_once(watcher.as_mut(), &waker).is_pending());

	services
		.threads
		.add_to_thread(&root.event_id, reply_id, &reply)
		.await?;

	assert!(observation.was_consistent());
	let participants = services
		.threads
		.get_participants(&root_id)
		.await?;

	assert_eq!(participants, [root.sender, reply.sender]);

	assert_eq!(stored(services, &root_id).await?["unsigned"]["age"], 42);

	Ok(())
}

#[tokio::test]
async fn participants_are_recorded_once() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!thread:localhost");
	let root_sender = user_id!("@root:localhost");
	let reply_sender = user_id!("@reply:localhost");
	let lone_sender = user_id!("@lone:localhost");
	let root = pdu(room_id, event_id!("$root:localhost"), root_sender);
	let first = pdu(room_id, event_id!("$first:localhost"), reply_sender);
	let again = pdu(room_id, event_id!("$again:localhost"), reply_sender);
	let own = pdu(room_id, event_id!("$own:localhost"), root_sender);
	let lone = pdu(room_id, event_id!("$lone:localhost"), lone_sender);
	let lone_own = pdu(room_id, event_id!("$lone_own:localhost"), lone_sender);
	let root_id = pdu_id(1);
	let lone_id = pdu_id(5);
	let _guard = services.state.mutex.lock(room_id).await;

	store(services, &root_id, &root);
	store(services, &lone_id, &lone);

	services
		.threads
		.add_to_thread(&root.event_id, pdu_id(2), &first)
		.await?;

	let waker = Waker::noop();
	let mut watcher = pin!(services.db["threadid_userids"].watch_raw_prefix_once(root_id)); // Future::poll requires mutable access.

	assert!(poll_once(watcher.as_mut(), waker).is_pending());

	// Replies from recorded senders leave the participant row unwritten.
	for (count, reply) in [(3, &again), (4, &own)] {
		services
			.threads
			.add_to_thread(&root.event_id, pdu_id(count), reply)
			.await?;
	}

	assert!(poll_once(watcher.as_mut(), waker).is_pending());

	let latest = services.db["threadrootid_latestcount"]
		.get(&root_id)
		.await?;

	assert_eq!(&*latest, &pdu_id(4).pdu_count().to_be_bytes());

	let thread = &stored(services, &root_id).await?["unsigned"]["m.relations"]["m.thread"];

	assert_eq!(thread["count"], 3);

	services
		.threads
		.add_to_thread(&lone.event_id, pdu_id(6), &lone_own)
		.await?;

	let participants = services
		.threads
		.get_participants(&root_id)
		.await?;

	assert_eq!(participants, [root.sender, first.sender]);

	let participants = services
		.threads
		.get_participants(&lone_id)
		.await?;

	assert_eq!(participants, [lone.sender]);

	Ok(())
}

fn pdu(room_id: &RoomId, event_id: &EventId, sender: &UserId) -> PduEvent {
	serde_json::from_value(json!({
		"type": "m.room.message",
		"event_id": event_id,
		"room_id": room_id,
		"sender": sender,
		"origin_server_ts": 1,
		"depth": 1,
		"hashes": { "sha256": "hash" },
		"prev_events": [],
		"auth_events": [],
		"content": { "msgtype": "m.text", "body": "reply" },
		"unsigned": { "age": 42 },
	}))
	.expect("test PDU")
}

fn store(services: &Services, pdu_id: &RawPduId, pdu: &PduEvent) {
	services.db["eventid_pduid"].insert(pdu.event_id.as_bytes(), pdu_id.as_bytes());
	services.db["pduid_pdu"].raw_put(pdu_id, Json(pdu));
}

async fn stored(services: &Services, pdu_id: &RawPduId) -> Result<Value> {
	let pdu = services
		.timeline
		.get_pdu_json_from_id(pdu_id)
		.await?;

	Ok(serde_json::to_value(pdu)?)
}

#[tokio::test]
async fn foreign_room_reply_preserves_the_existing_thread() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!thread:localhost");
	let root = pdu(room_id, event_id!("$root:localhost"), user_id!("@root:localhost"));
	let reply = pdu(room_id, event_id!("$reply:localhost"), user_id!("@reply:localhost"));
	let foreign = pdu(
		room_id!("!foreign:localhost"),
		event_id!("$foreign:localhost"),
		user_id!("@foreign:localhost"),
	);

	let root_id = pdu_id(1);
	let reply_id = pdu_id(2);
	let foreign_id = PduId {
		shortroomid: 2,
		count: PduCount::Normal(3),
	}
	.into();
	let guard = services.state.mutex.lock(room_id).await;

	store(services, &root_id, &root);
	services
		.threads
		.add_to_thread(&root.event_id, reply_id, &reply)
		.await?;

	let before = stored(services, &root_id).await?;

	drop(guard);

	let _guard = services.state.mutex.lock(&foreign.room_id).await;

	services
		.threads
		.add_to_thread(&root.event_id, foreign_id, &foreign)
		.await?;

	assert_eq!(stored(services, &root_id).await?, before);
	let participants = services
		.threads
		.get_participants(&root_id)
		.await?;

	assert_eq!(participants, [root.sender, reply.sender]);

	assert!(
		services.db["threadactivityid_rootid"]
			.get(&foreign_id)
			.await
			.is_not_found()
	);

	let latest = services.db["threadrootid_latestcount"]
		.get(&root_id)
		.await?;

	assert_eq!(&*latest, &reply_id.pdu_count().to_be_bytes());

	Ok(())
}

#[tokio::test]
async fn backfilled_reply_preserves_normal_activity_pointer() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!thread:localhost");
	let root = pdu(room_id, event_id!("$root:localhost"), user_id!("@root:localhost"));
	let reply = pdu(room_id, event_id!("$reply:localhost"), user_id!("@reply:localhost"));
	let earlier = pdu(room_id, event_id!("$earlier:localhost"), user_id!("@earlier:localhost"));
	let root_id = pdu_id(1);
	let reply_id = pdu_id(9);
	let earlier_id = PduId {
		shortroomid: 1,
		count: PduCount::Backfilled(-1),
	}
	.into();
	let _guard = services.state.mutex.lock(room_id).await;

	store(services, &root_id, &root);
	services
		.threads
		.add_to_thread(&root.event_id, reply_id, &reply)
		.await?;

	services
		.threads
		.add_to_thread(&root.event_id, earlier_id, &earlier)
		.await?;

	assert!(
		services.db["threadactivityid_rootid"]
			.get(&earlier_id)
			.await
			.is_not_found()
	);

	let latest = services.db["threadrootid_latestcount"]
		.get(&root_id)
		.await?;

	assert_eq!(&*latest, &reply_id.pdu_count().to_be_bytes());

	let activity = services.db["threadactivityid_rootid"]
		.get(&reply_id)
		.await?;

	assert_eq!(&*activity, root_id.as_bytes());

	let participants = services
		.threads
		.get_participants(&root_id)
		.await?;

	assert_eq!(participants, [root.sender, reply.sender, earlier.sender]);

	assert_eq!(
		stored(services, &root_id).await?["unsigned"]["m.relations"]["m.thread"]["count"],
		2,
	);

	Ok(())
}
