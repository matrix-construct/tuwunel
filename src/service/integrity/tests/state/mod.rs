//! Room state rows that do not decode.
//!
//! The live state readers slice a state diff without checking its length, decode
//! the room's state pointer with the typed decoder, and keep a cache of rebuilt
//! states, so each check reaching an event through a room's state rebuilds it
//! from storage and reports such a row. The room version route then reads the
//! create event's accepted copy before its outlier, as the live reader does.

use ruma::{RoomId, RoomVersionId, event_id, room_id, user_id};
use serde_json::json;
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::Json;
use tuwunel_matrix::PduCount;

use super::{overlong, seed_room_state, store, walk};
use crate::{
	Service, Services,
	migrations::fresh,
	rooms::timeline::Mapped,
	test_utils::{fixture, pdu_json, with_field, with_state_key},
};

#[tokio::test]
async fn checks_report_a_truncated_state_diff() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!room:localhost");

	seed(services, room).await?;

	let shortstatehash = services
		.state
		.get_room_shortstatehash(room)
		.await?;

	services.db["shortstatehash_statediff"].insert(&shortstatehash.to_be_bytes(), [0; 4]);
	services.state_compressor.clear_cache().await;

	assert_malformed(services).await
}

#[tokio::test]
async fn checks_report_an_overlong_state_pointer() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!room:localhost");

	seed(services, room).await?;

	let shortstatehash = services
		.state
		.get_room_shortstatehash(room)
		.await?;

	let overlong = overlong(shortstatehash);

	services.db["roomid_shortstatehash"].insert(room.as_bytes(), overlong);
	services.state_compressor.clear_cache().await;

	assert_malformed(services).await
}

#[tokio::test]
async fn room_version_reads_the_accepted_create_copy_first() -> Result {
	let config = Figment::new()
		.merge(("admin.room.create", false))
		.merge(("cache.pdu_capacity", 0))
		.merge(("rocksdb.cache_capacity_mb", 0.0));

	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!room:localhost");
	let alice = user_id!("@alice:localhost");
	let create = event_id!("$create:localhost");
	let content = |version: &str| json!({ "creator": alice, "room_version": version });
	let outlier = pdu_json(create, room, alice, "m.room.create", &content("11"));
	let state = [(1, create, "m.room.create", "", content("10"))];

	fresh(services).await?;
	seed_room_state(services, room, alice, &state).await?;

	// With no event cache, the flushed accepted copy waits on a disk read while the
	// outlier copy is in the memtable, so a race would read the outlier first.
	services.db["pduid_pdu"].sort()?;
	services.db["eventid_outlierpdu"].raw_put(create, Json(with_state_key(outlier, "")));

	let version = services.state.room_version_checked(room).await?;

	assert!(matches!(version, Mapped::Event(RoomVersionId::V10)));

	Ok(())
}

/// Seeds a room that both checks reach through its current state.
///
/// The state holds a create event and a joined member, and the room holds one
/// redaction, on a database an origin recording redactions by reference opened.
async fn seed(services: &Services, room: &RoomId) -> Result {
	let alice = user_id!("@alice:localhost");
	let redaction = event_id!("$redaction:localhost");
	let create = json!({ "creator": alice, "room_version": "10" });
	let state = [
		(1, event_id!("$create:localhost"), "m.room.create", "", create),
		(
			2,
			event_id!("$alice:localhost"),
			"m.room.member",
			alice.as_str(),
			json!({ "membership": "join" }),
		),
	];

	let redacts = pdu_json(redaction, room, alice, "m.room.redaction", &json!({}));
	let redacts = with_field(redacts, "redacts", json!("$target:localhost"));

	fresh(services).await?;
	services.db["global"].insert("unembed_unsigned_info", []);
	seed_room_state(services, room, alice, &state).await?;
	services
		.state_cache
		.mark_as_joined(alice, room, PduCount::Normal(3));

	let shortroomid = services.short.get_shortroomid(room).await?;

	store(services, shortroomid, 4, redaction, &redacts);

	Ok(())
}

async fn assert_malformed(services: &Services) -> Result {
	for check in ["joined-members", "unrecorded-redactions"] {
		let summary = walk(services, check).await?;

		assert_eq!(summary.counts.get("malformed-mapping"), Some(&1), "{check}");
		assert_eq!(summary.findings(), 1, "{check}");
	}

	Ok(())
}
