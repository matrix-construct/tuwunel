//! Data a foreign database left in a shape this server does not read.
//!
//! Conduit leaves events without their room and last-read markers in the
//! highlight-count column, and other origins leave redactions unrecorded and
//! moderation in columns of their own.

use ruma::{EventId, RoomId, event_id, room_id, user_id};
use serde_json::{Value, json};
use tuwunel_core::{Err, Result, config::Figment};
use tuwunel_database::Json;

use super::{count, present, room_pdu_id, run, seed_room_state, store, walk};
use crate::{
	integrity::{
		Report,
		moderation::{Finding as Moderation, entry as moderation_entry, finding as judge},
	},
	migrations::{fresh, migrate},
	test_utils::{fixture, pdu_id, pdu_json, with_field},
};

#[tokio::test]
async fn pdu_room_ids_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let (pdus, outliers) = (&services.db["pduid_pdu"], &services.db["eventid_outlierpdu"]);
	let room = room_id!("!room:localhost");
	let create = event_id!("$Rqnc-F-dvnEYJTyHq_iKxU2bZ1CI92-kuZq3a5lr5Zg");
	let rootless = |kind: &str| Json(json!({ "type": kind, "content": {} }));
	let room_of =
		|pdu: &[u8]| serde_json::from_slice(pdu).map(|pdu: Value| pdu["room_id"].clone());

	fresh(services).await?;
	services.db["roomid_shortroomid"].insert(room.as_bytes(), 1_u64.to_be_bytes());
	pdus.raw_put(pdu_id(1), rootless("m.room.message"));
	pdus.raw_put(room_pdu_id(2, 1), rootless("m.room.message"));
	pdus.insert(&pdu_id(2), b"not json");
	pdus.raw_put(pdu_id(3), Json(json!({ "room_id": room })));
	outliers.raw_put(create.as_bytes(), rootless("m.room.create"));
	outliers.raw_put(b"$message:localhost", rootless("m.room.message"));
	outliers.insert(b"$junk:localhost", b"not json");

	let summary = run(services, "pdu-room-ids").await?;

	assert_eq!(summary.counts.get("missing-room-id"), Some(&2));
	assert_eq!(summary.counts.get("unresolved-room"), Some(&2));
	assert_eq!(summary.counts.get("unparsable-event"), Some(&1));
	assert_eq!(summary.findings(), 5);

	services.db["global"].remove("fix_pdu_missing_room_id");
	migrate(services, false).await?;

	let summary = run(services, "pdu-room-ids").await?;

	assert_eq!(summary.counts.get("unresolved-room"), Some(&2));
	assert_eq!(summary.counts.get("unparsable-event"), Some(&1));
	assert_eq!(summary.findings(), 3);
	assert_eq!(room_of(&pdus.get(&pdu_id(1)).await?)?, room.as_str());
	assert_eq!(
		room_of(&outliers.get(create.as_bytes()).await?)?,
		"!Rqnc-F-dvnEYJTyHq_iKxU2bZ1CI92-kuZq3a5lr5Zg"
	);

	Ok(())
}

#[tokio::test]
async fn highlight_keys_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let highlight = &services.db["userroomid_highlightcount"];
	let lastread = &services.db["roomuserid_lastnotificationread"];
	let room_keyed = b"!room:localhost\xFF@alice:localhost";
	let user_keyed = b"@alice:localhost\xFF!room:localhost";
	let below = b" stray";

	fresh(services).await?;
	highlight.insert(room_keyed.as_slice(), 5_u64.to_be_bytes());
	highlight.insert(user_keyed.as_slice(), 3_u64.to_be_bytes());
	highlight.insert(below.as_slice(), 1_u64.to_be_bytes());

	assert_eq!(count(services, "highlight-keys").await?, 1);

	services.db["global"].remove("split_conduit_highlight");
	migrate(services, false).await?;

	assert_eq!(count(services, "highlight-keys").await?, 0);
	assert_eq!(&*lastread.get(room_keyed.as_slice()).await?, 5_u64.to_be_bytes().as_slice());
	assert!(!present(highlight, room_keyed).await);
	assert!(present(highlight, user_keyed).await);
	assert!(present(highlight, below).await);
	assert!(!present(lastread, below).await);

	Ok(())
}

#[tokio::test]
async fn unrecorded_redactions_report_a_redaction_without_canonical_json() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!room:localhost");
	let alice = user_id!("@alice:localhost");
	let (target, redaction) = (event_id!("$target:localhost"), event_id!("$redaction:localhost"));
	let create = json!({ "creator": alice, "room_version": "10" });
	let state = [(1, event_id!("$create:localhost"), "m.room.create", "", create)];

	// Canonical JSON has no floats, so the redaction cannot be recorded as it is.
	let redacts = pdu_json(redaction, room, alice, "m.room.redaction", &json!({ "n": 1.5 }));
	let redacts = with_field(redacts, "redacts", json!(target));
	let emptied = pdu_json(target, room, alice, "m.room.message", &json!({}));

	fresh(services).await?;
	services.db["global"].insert("unembed_unsigned_info", []);
	seed_room_state(services, room, alice, &state).await?;

	let shortroomid = services.short.get_shortroomid(room).await?;

	store(services, shortroomid, 2, target, &emptied);
	store(services, shortroomid, 3, redaction, &redacts);

	let summary = walk(services, "unrecorded-redactions").await?;

	assert_eq!(summary.counts.get("undecidable-redaction"), Some(&1));
	assert_eq!(summary.findings(), 1);

	Ok(())
}

#[tokio::test]
async fn unrecorded_redactions_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let (room, unknown) = (room_id!("!room:localhost"), room_id!("!unknown:localhost"));
	let alice = user_id!("@alice:localhost");
	let target = event_id!("$target:localhost");
	let (redaction, stray) = (event_id!("$redaction:localhost"), event_id!("$stray:localhost"));
	let create = json!({ "creator": alice, "room_version": "10" });
	let state = [(1, event_id!("$create:localhost"), "m.room.create", "", create)];
	let redacts = |id: &EventId, room: &RoomId| {
		let pdu = pdu_json(id, room, alice, "m.room.redaction", &json!({}));

		with_field(pdu, "redacts", json!(target))
	};

	fresh(services).await?;
	services.db["global"].insert("unembed_unsigned_info", []);
	seed_room_state(services, room, alice, &state).await?;

	let shortroomid = services.short.get_shortroomid(room).await?;
	let unknown_short = services
		.short
		.get_or_create_shortroomid(unknown)
		.await;

	let emptied = pdu_json(target, room, alice, "m.room.message", &json!({}));

	store(services, shortroomid, 2, target, &emptied);
	store(services, shortroomid, 3, redaction, &redacts(redaction, room));
	store(services, unknown_short, 1, stray, &redacts(stray, unknown));
	services.db["pduid_pdu"]
		.raw_put(room_pdu_id(shortroomid, 4), Json(json!({ "type": "m.room.redaction" })));

	let summary = walk(services, "unrecorded-redactions").await?;

	assert_eq!(summary.counts.get("unrecorded-redaction"), Some(&1));
	assert_eq!(summary.counts.get("unparsable-redaction"), Some(&1));
	assert_eq!(summary.counts.get("unknown-room-version"), Some(&1));
	assert_eq!(summary.findings(), 3);

	services.db["global"].remove("restore_foreign_redactions");
	migrate(services, false).await?;

	let summary = walk(services, "unrecorded-redactions").await?;
	let restored = services
		.timeline
		.get_pdu_json_from_id(&room_pdu_id(shortroomid, 2))
		.await?;

	assert_eq!(summary.counts.get("unparsable-redaction"), Some(&1));
	assert_eq!(summary.counts.get("unknown-room-version"), Some(&1));
	assert_eq!(summary.findings(), 2);
	assert_eq!(
		serde_json::to_value(restored)?["unsigned"]["redacted_because"]["event_id"],
		redaction.as_str()
	);

	Ok(())
}

#[tokio::test]
async fn unrecorded_redactions_read_a_target_as_the_repair_does() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!room:localhost");
	let alice = user_id!("@alice:localhost");
	let (target, redaction) = (event_id!("$target:localhost"), event_id!("$redaction:localhost"));
	let create = json!({ "creator": alice, "room_version": "10" });
	let state = [(1, event_id!("$create:localhost"), "m.room.create", "", create)];
	let redacts = pdu_json(redaction, room, alice, "m.room.redaction", &json!({}));
	let redacts = with_field(redacts, "redacts", json!(target));
	let emptied = pdu_json(target, room, alice, "m.room.message", &json!({}));

	// Both builds of the repair's decoder read the event and ignore what follows it.
	let trailing = [serde_json::to_vec(&emptied)?.as_slice(), b"junk"].concat();

	fresh(services).await?;
	services.db["global"].insert("unembed_unsigned_info", []);
	seed_room_state(services, room, alice, &state).await?;

	let shortroomid = services.short.get_shortroomid(room).await?;

	store(services, shortroomid, 2, target, &emptied);
	store(services, shortroomid, 3, redaction, &redacts);
	services.db["pduid_pdu"].insert(&room_pdu_id(shortroomid, 2), trailing);

	let summary = walk(services, "unrecorded-redactions").await?;

	assert_eq!(summary.counts.get("unrecorded-redaction"), Some(&1));
	assert_eq!(summary.findings(), 1);

	services.db["global"].remove("restore_foreign_redactions");
	migrate(services, false).await?;

	let summary = walk(services, "unrecorded-redactions").await?;
	let restored = services
		.timeline
		.get_pdu_json_from_id(&room_pdu_id(shortroomid, 2))
		.await?;

	assert_eq!(summary.findings(), 0);
	assert_eq!(
		serde_json::to_value(restored)?["unsigned"]["redacted_because"]["event_id"],
		redaction.as_str()
	);

	Ok(())
}

#[test]
fn moderation_entries_classify_against_their_copy() -> Result {
	let fallback = user_id!("@import:localhost");
	let active = br#"{"suspended":true,"suspended_at":1700000000000,"suspended_by":"@mod:example.org","reason":"spam"}"#;
	let classify = |value: &[u8], current: Option<&[u8]>| {
		moderation_entry(b"@alice:localhost", value, fallback)
			.and_then(|entry| judge("userid_suspension", entry, current))
	};

	let kind = |value: &[u8], current: Option<&[u8]>| {
		classify(value, current)
			.as_ref()
			.map(Moderation::kind)
	};

	let Some(Moderation::Uncopied { moderation, .. }) = classify(active, None) else {
		return Err!("an active entry with no copy is uncopied");
	};

	let copied = serde_json::to_vec(&moderation)?;

	assert_eq!(u64::from(moderation.when.get()), 1_700_000_000_000);
	assert_eq!(moderation.by.as_str(), "@mod:example.org");
	assert_eq!(kind(active, Some(&copied)), None);
	assert_eq!(kind(active, Some(b"{}")), Some("uncopied-entry"));
	assert_eq!(kind(b"not json", None), Some("unparsable-entry"));
	assert_eq!(
		kind(br#"{"suspended":false,"suspended_at":1,"suspended_by":"@a:b.c"}"#, None),
		None
	);

	let unparsable_actor =
		br#"{"suspended":true,"suspended_at":1,"suspended_by":"not-a-user-id"}"#;

	let Some(Moderation::Uncopied { moderation, .. }) = classify(unparsable_actor, None) else {
		return Err!("an entry with an unparsable actor is still copied");
	};

	assert_eq!(moderation.by, fallback);

	Ok(())
}
