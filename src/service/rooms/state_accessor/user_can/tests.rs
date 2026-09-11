use std::sync::Arc;

use ruma::{EventId, RoomId, UserId, event_id, events::StateEventType, room_id, user_id};
use serde_json::{Value, json};
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::Json;

use crate::{Services, test_utils::fixture};

struct Ids {
	room: &'static RoomId,
	foreign: &'static RoomId,
	owner: &'static UserId,
	author: &'static UserId,
	peer: &'static UserId,
	create: &'static EventId,
	local_event: &'static EventId,
	foreign_event: &'static EventId,
	acl: &'static EventId,
	unknown: &'static EventId,
}

#[tokio::test]
async fn redaction_authority_does_not_cross_room_boundaries() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	let ids = seed(services).await?;

	for federation in [false, true] {
		for sender in [ids.owner, ids.author, ids.peer] {
			assert!(
				!services
					.state_accessor
					.user_can_redact(ids.foreign_event, sender, ids.room, federation)
					.await?
			);
		}

		for (sender, allowed) in [(ids.owner, true), (ids.author, true), (ids.peer, federation)] {
			let can_redact = services
				.state_accessor
				.user_can_redact(ids.local_event, sender, ids.room, federation)
				.await?;

			assert_eq!(can_redact, allowed);
		}

		assert!(
			services
				.state_accessor
				.user_can_redact(ids.unknown, ids.owner, ids.room, federation)
				.await?
		);

		for protected in [ids.create, ids.acl] {
			services
				.state_accessor
				.user_can_redact(protected, ids.owner, ids.room, federation)
				.await
				.expect_err("protected state event");
		}
	}

	Ok(())
}

async fn seed(services: &Services) -> Result<Ids> {
	let ids = Ids {
		room: room_id!("!redaction:localhost"),
		foreign: room_id!("!other:localhost"),
		owner: user_id!("@owner:localhost"),
		author: user_id!("@author:remote.invalid"),
		peer: user_id!("@peer:remote.invalid"),
		create: event_id!("$create:localhost"),
		local_event: event_id!("$local:localhost"),
		foreign_event: event_id!("$foreign:localhost"),
		acl: event_id!("$acl:localhost"),
		unknown: event_id!("$unknown:localhost"),
	};

	let content = json!({
		"creator": ids.owner, "room_version": "10",
	});

	let create = state_event(event(ids.create, ids.room, ids.owner, "m.room.create", &content));

	services.db["eventid_outlierpdu"].raw_put(ids.create, Json(create));
	let content = json!({
		"allow": ["*"], "deny": [], "allow_ip_literals": false,
	});

	let acl = state_event(event(ids.acl, ids.room, ids.owner, "m.room.server_acl", &content));

	services.db["eventid_outlierpdu"].raw_put(ids.acl, Json(acl));

	for (id, room) in [(ids.local_event, ids.room), (ids.foreign_event, ids.foreign)] {
		let content = json!({
			"msgtype": "m.text", "body": "preserve this message",
		});

		let pdu = event(id, room, ids.author, "m.room.message", &content);

		services.db["eventid_outlierpdu"].raw_put(id, Json(pdu));
	}

	let key = services
		.short
		.get_or_create_shortstatekey(&StateEventType::RoomCreate, "")
		.await;

	let create = services
		.state_compressor
		.compress_state_event(key, ids.create)
		.await;

	let state = Arc::new([create].into());
	let state = services
		.state
		.set_event_state(ids.create, ids.room, state)
		.await?;

	let lock = services.state.mutex.lock(ids.room).await;

	services
		.state
		.set_room_state(ids.room, state, &lock);

	Ok(ids)
}

fn state_event(mut pdu: Value) -> Value {
	pdu["state_key"] = json!("");
	pdu
}

fn event(id: &EventId, room: &RoomId, sender: &UserId, kind: &str, content: &Value) -> Value {
	json!({
		"event_id": id, "room_id": room, "sender": sender, "type": kind, "content": content,
		"origin": "localhost", "origin_server_ts": 1, "depth": 1,
		"prev_events": [], "auth_events": [], "signatures": {},
		"hashes": {"sha256": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"},
	})
}
