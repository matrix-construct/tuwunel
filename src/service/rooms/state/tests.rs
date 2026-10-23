//! Tests the invite and knock room summary against seeded room state.
//!
//! A state entry whose event is missing from storage, one whose short event ID
//! no longer maps back, and one naming a stored stripped event each warn and
//! are omitted. A key absent from the state is skipped quietly, and a room
//! with no snapshot warns once.

use std::sync::Arc;

use ruma::{
	CanonicalJsonObject, EventId, RoomId, RoomVersionId, UserId, event_id,
	events::StateEventType, room_id, user_id,
};
use serde_json::{Value, json};
use tuwunel_core::{Result, config::Figment};
use tuwunel_matrix::PduEvent;

use crate::{
	Services,
	rooms::state_compressor::CompressedStateEvent,
	test_utils::{Warnings, compress_state, fixture, pdu_json, store_outlier, with_state_key},
};

type Summary = (Vec<Value>, Warnings);

#[tokio::test]
async fn summary_logs_unreadable_entries_and_skips_absent_ones() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let missing = event_id!("$missing-topic:localhost");
	let unmapped = event_id!("$unmapped-join-rules:localhost");
	let stripped = event_id!("$stripped-avatar:localhost");

	seed(services, missing, unmapped, stripped).await?;

	let (summary, warnings) = summarize(services).await?;

	let types: Vec<_> = summary
		.iter()
		.map(|served| &served["type"])
		.collect();

	assert_eq!(
		types,
		["m.room.create", "m.room.encryption", "m.room.member"],
		"only the stored PDUs and the invite are served, in order"
	);

	assert_eq!(warnings.count(), 3, "only the three unreadable entries warn");
	assert!(warnings.warned("event_id", missing.as_str()));
	assert!(warnings.warned("event_id", stripped.as_str()));
	assert!(warnings.warned("event_type", "m.room.join_rules"));

	Ok(())
}

#[tokio::test]
async fn summary_warns_once_for_a_room_without_a_snapshot() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let (summary, warnings) = summarize(&fixture.services).await?;

	assert_eq!(summary.len(), 1, "only the invite is served");
	assert_eq!(warnings.count(), 1, "one warning covers the whole snapshot");
	assert!(warnings.warned("room_id", room().as_str()));

	Ok(())
}

async fn seed(
	services: &Services,
	missing: &EventId,
	unmapped: &EventId,
	stripped: &EventId,
) -> Result {
	let room_id = room();
	let create_id = event_id!("$create:localhost");
	let create_content = json!({"room_version": "11"});
	let encryption_id = event_id!("$encryption:localhost");
	let encryption_content = json!({"algorithm": "m.megolm.v1.aes-sha2"});
	let avatar_type = StateEventType::RoomAvatar;
	let stripped_state = json!({
		"event_id": stripped, "room_id": room_id, "type": avatar_type, "state_key": "",
		"sender": sender(), "content": {},
	});

	// Interned but absent from the state, so its lookup misses the snapshot.
	services
		.short
		.get_or_create_shortstatekey(&StateEventType::RoomName, "")
		.await;

	let create = store(services, &StateEventType::RoomCreate, create_id, &create_content).await?;
	let join_rules = compress_state(services, &StateEventType::RoomJoinRules, "", unmapped).await;

	// Stored the way a knock stores the stripped state it receives.
	let avatar = store_outlier(services, &avatar_type, "", stripped, stripped_state).await?;
	let encryption =
		store(services, &StateEventType::RoomEncryption, encryption_id, &encryption_content)
			.await?;

	let topic = compress_state(services, &StateEventType::RoomTopic, "", missing).await;
	let state = [create, join_rules, avatar, encryption, topic];
	let shortstatehash = services
		.state
		.set_event_state(create_id, room_id, Arc::new(state.into()))
		.await?;

	let lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_room_state(room_id, shortstatehash, &lock);

	let shorteventid = services
		.short
		.get_or_create_shorteventid(unmapped)
		.await;

	services.db["shorteventid_eventid"].del(shorteventid);

	Ok(())
}

async fn store(
	services: &Services,
	event_type: &StateEventType,
	event_id: &EventId,
	content: &Value,
) -> Result<CompressedStateEvent> {
	let pdu = event(event_type, event_id, "", content);

	store_outlier(services, event_type, "", event_id, pdu).await
}

fn event(
	event_type: &StateEventType,
	event_id: &EventId,
	state_key: &str,
	content: &Value,
) -> Value {
	with_state_key(pdu_json(event_id, room(), sender(), event_type, content), state_key)
}

async fn summarize(services: &Services) -> Result<Summary> {
	let (membership, membership_json) = invite()?;
	let warnings = Warnings::capture();
	let summary = services
		.state
		.summary_pdus(&membership, &membership_json, &RoomVersionId::V11)
		.await
		.iter()
		.map(|served| serde_json::from_str(served.get()))
		.collect::<Result<_, _>>()?;

	Ok((summary, warnings))
}

fn invite() -> Result<(PduEvent, CanonicalJsonObject)> {
	let content = json!({"membership": "invite"});
	let invite = event(
		&StateEventType::RoomMember,
		event_id!("$invite:localhost"),
		"@invitee:remote.invalid",
		&content,
	);

	Ok((serde_json::from_value(invite.clone())?, serde_json::from_value(invite)?))
}

fn room() -> &'static RoomId { room_id!("!summary:localhost") }

fn sender() -> &'static UserId { user_id!("@sender:localhost") }
