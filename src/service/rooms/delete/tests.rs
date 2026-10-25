use ruma::{CanonicalJsonObject, event_id, room_id, user_id};
use serde_json::{from_value, json, to_vec};
use tuwunel_core::{Result, config::Figment};

use crate::test_utils::{fixture, pdu_id, pdu_json};

#[tokio::test]
async fn purge_clears_the_rooms_soft_fail_markers() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let timeline = &services.timeline;
	let metadata = &services.pdu_metadata;
	let room = room_id!("!purged:localhost");
	let foreign_room = room_id!("!foreign:localhost");
	let sender = user_id!("@sender:localhost");
	let accepted = event_id!("$accepted:localhost");
	let outlier = event_id!("$outlier:localhost");
	let foreign = event_id!("$foreign:localhost");
	let unreadable = event_id!("$unreadable:localhost");
	let id = pdu_id(1);
	let content = json!({"msgtype": "m.text", "body": "purge"});
	let pdu = pdu_json(accepted, room, sender, "m.room.message", &content);

	services.db["roomid_shortroomid"].insert(room, 1_u64.to_be_bytes());
	services.db["pduid_pdu"].insert(&id, to_vec(&pdu)?);
	services.db["eventid_pduid"].insert(accepted, id.as_bytes());
	for (event, owner) in [(outlier, room), (foreign, foreign_room)] {
		let pdu = pdu_json(event, owner, sender, "m.room.message", &content);
		let pdu: CanonicalJsonObject = from_value(pdu)?;

		timeline.add_pdu_outlier(event, &pdu);
	}

	for event in [accepted, outlier, foreign, unreadable] {
		metadata.mark_event_soft_failed(event);
	}

	let lock = services.state.mutex.lock(room).await;

	services
		.delete
		.delete_room(room, false, lock)
		.await?;

	assert!(!timeline.pdu_exists(accepted).await, "purge deletes the accepted event");
	assert!(
		!metadata.is_event_soft_failed(outlier).await,
		"purge clears the outlier's marker"
	);

	assert!(
		!metadata.is_event_soft_failed(accepted).await,
		"purge clears the accepted marker before deleting its event"
	);

	assert!(metadata.is_event_soft_failed(foreign).await, "another room's marker survives");

	assert!(
		metadata.is_event_soft_failed(unreadable).await,
		"an unattributable marker survives"
	);

	Ok(())
}
