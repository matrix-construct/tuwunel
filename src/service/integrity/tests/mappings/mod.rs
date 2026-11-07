//! Timeline mappings of the wrong length.
//!
//! The live event readers panic on an `eventid_pduid` value that is not a PDU id,
//! so each check reaching an event through one reads it untrusted and reports it.

use ruma::{event_id, room_id, user_id};
use serde_json::json;
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::Json;
use tuwunel_matrix::PduCount;

use super::{seed_room_state, store, walk};
use crate::{
	migrations::{fresh, migrate},
	test_utils::{fixture, pdu_id, pdu_json, with_field, with_state_key},
};

const DAMAGED: &[u8] = b"bad";

#[tokio::test]
async fn unrecorded_redactions_report_a_malformed_create_mapping() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!room:localhost");
	let alice = user_id!("@alice:localhost");
	let (create, redaction) = (event_id!("$create:localhost"), event_id!("$redaction:localhost"));
	let content = json!({ "creator": alice, "room_version": "10" });
	let state = [(1, create, "m.room.create", "", content)];
	let redacts = pdu_json(redaction, room, alice, "m.room.redaction", &json!({}));
	let redacts = with_field(redacts, "redacts", json!("$target:localhost"));

	fresh(services).await?;
	services.db["global"].insert("unembed_unsigned_info", []);
	seed_room_state(services, room, alice, &state).await?;

	let shortroomid = services.short.get_shortroomid(room).await?;

	store(services, shortroomid, 2, redaction, &redacts);
	services.db["eventid_pduid"].insert(create.as_bytes(), DAMAGED);

	let summary = walk(services, "unrecorded-redactions").await?;

	assert_eq!(summary.counts.get("malformed-mapping"), Some(&1));
	assert_eq!(summary.findings(), 1);

	Ok(())
}

#[tokio::test]
async fn thread_summaries_report_a_malformed_latest_mapping() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let latest = event_id!("$latest:localhost");
	let thread = json!({ "m.relations": { "m.thread": { "count": 0, "latest_event": { "event_id": latest } } } });
	let root = json!({ "room_id": "!thread:localhost", "unsigned": thread });

	fresh(services).await?;
	services.db["pduid_pdu"].raw_put(pdu_id(1), Json(&root));
	services.db["threadid_userids"].insert(&pdu_id(1), "@alice:localhost");
	services.db["eventid_pduid"].insert(latest.as_bytes(), DAMAGED);

	let summary = walk(services, "thread-summaries").await?;

	assert_eq!(summary.counts.get("malformed-mapping"), Some(&1));
	assert_eq!(summary.findings(), 1);

	// The rebuild shares the recount, so it leaves the root as it is and counts it failed.
	services.db["global"].remove("rebuild_thread_summaries");
	migrate(services, false).await?;

	let summary = walk(services, "thread-summaries").await?;
	let stored = services
		.timeline
		.get_pdu_json_from_id(&pdu_id(1))
		.await?;

	assert_eq!(summary.counts.get("malformed-mapping"), Some(&1));
	assert_eq!(serde_json::to_value(stored)?, root);

	Ok(())
}

#[tokio::test]
async fn joined_members_report_a_malformed_member_mapping() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!room:localhost");
	let alice = user_id!("@alice:localhost");
	let member = event_id!("$alice:localhost");
	let content = json!({ "creator": alice, "room_version": "10" });
	let joined = json!({ "membership": "join" });
	let state = [
		(1, event_id!("$create:localhost"), "m.room.create", "", content),
		(2, member, "m.room.member", alice.as_str(), joined),
	];

	fresh(services).await?;
	seed_room_state(services, room, alice, &state).await?;
	services
		.state_cache
		.mark_as_joined(alice, room, PduCount::Normal(3));

	services.db["eventid_pduid"].insert(member.as_bytes(), DAMAGED);

	let summary = walk(services, "joined-members").await?;

	assert_eq!(summary.counts.get("malformed-mapping"), Some(&1));
	assert_eq!(summary.findings(), 1);

	Ok(())
}

#[tokio::test]
async fn joined_members_read_an_outlier_copy_found_before_a_damaged_mapping() -> Result {
	let config = Figment::new()
		.merge(("admin.room.create", false))
		.merge(("cache.eventid_pdu_capacity", 0))
		.merge(("rocksdb.cache_capacity_mb", 0.0));

	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!room:localhost");
	let alice = user_id!("@alice:localhost");
	let member = event_id!("$alice:localhost");
	let content = json!({ "creator": alice, "room_version": "10" });
	let joined = json!({ "membership": "join" });
	let outlier = pdu_json(member, room, alice, "m.room.member", &joined);
	let outlier = with_state_key(outlier, alice.as_str());
	let state = [
		(1, event_id!("$create:localhost"), "m.room.create", "", content),
		(2, member, "m.room.member", alice.as_str(), joined),
	];

	fresh(services).await?;
	seed_room_state(services, room, alice, &state).await?;
	services
		.state_cache
		.mark_as_joined(alice, room, PduCount::Normal(3));

	// With no mapping cache, the flushed damaged mapping waits on a disk read while
	// the outlier copy is in the memtable, so the outlier is found first, as `get` finds it.
	services.db["eventid_pduid"].insert(member.as_bytes(), DAMAGED);
	services.db["eventid_pduid"].sort()?;
	services.db["eventid_outlierpdu"].raw_put(member, Json(outlier));

	let summary = walk(services, "joined-members").await?;

	assert_eq!(summary.findings(), 0);

	Ok(())
}
