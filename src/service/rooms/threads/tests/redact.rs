//! Redacting a thread reply takes it off its root's `m.thread.count`, and the
//! recount repairs counts kept before that.

use std::{
	pin::pin,
	sync::{Arc, Mutex},
	task::{Context, Wake, Waker},
};

use ruma::{EventId, OwnedEventId, RoomId, event_id, events::StateEventType, room_id};
use serde_json::{Value, json};
use tuwunel_core::{
	Event, Result,
	config::Figment,
	matrix::pdu::{PduCount, PduEvent, PduId, RawPduId},
};
use tuwunel_database::Json;

use crate::{
	Services,
	rooms::{short::ShortRoomId, threads::thread_root},
	test_utils::fixture,
};

/// A room with just its create event, so redaction can read its version.
struct Room<'a>(&'a Services, ShortRoomId, &'static RoomId);

/// The root's stored count when the redacted reply's write is announced.
struct CountAtReplyWrite(Arc<Services>, RawPduId, Mutex<Value>);

#[tokio::test]
async fn redaction_takes_each_thread_reply_off_once() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let room = Room::new(&fixture.services).await?;
	let [root, first, legacy, note, fix] = ["root", "first", "legacy", "note", "fix"].map(id);

	room.append(1, &root, text()).await?;
	room.append(2, &first, thread(&root)).await?;
	room.append(3, &legacy, legacy_reply(&root))
		.await?;
	room.append(4, &note, text()).await?;
	room.append(5, &fix, edit(&first)).await?;

	for (event, count) in [(&note, 2), (&fix, 2), (&first, 1), (&first, 1), (&legacy, 0)] {
		room.redact(event).await?;

		assert_eq!(room.count(&root).await?, count, "redacting {event}");
	}

	Ok(())
}

#[tokio::test]
async fn redacted_reply_and_root_count_are_written_together() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = Room::new(services).await?;
	let [root, first] = ["root", "first"].map(id);
	let root_id = room.append(1, &root, text()).await?;
	let first_id = room.append(2, &first, thread(&root)).await?;

	let observed = Arc::new(CountAtReplyWrite(services.clone(), root_id, Mutex::default()));
	let waker = Waker::from(observed.clone());
	let mut watcher = pin!(services.db["pduid_pdu"].watch_raw_prefix_once(first_id));
	let mut context = Context::from_waker(&waker);

	assert!(watcher.as_mut().poll(&mut context).is_pending());

	room.redact(&first).await?;

	assert_eq!(*observed.2.lock().expect("locked"), 0);

	Ok(())
}

#[tokio::test]
async fn recount_fixes_a_stale_root_and_leaves_a_correct_one() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let room = Room::new(&fixture.services).await?;
	let [stale, live, gone, kept, answer, fix] =
		["stale", "live", "gone", "kept", "answer", "fix"].map(id);

	room.append(1, &stale, text()).await?;
	room.append(2, &live, thread(&stale)).await?;
	room.append(3, &gone, thread(&stale)).await?;
	room.redact(&gone).await?;
	room.set_count(&stale, 2).await?;

	room.append(4, &kept, text()).await?;
	room.append(5, &answer, legacy_reply(&kept))
		.await?;
	room.append(6, &fix, edit(&kept)).await?;

	let threads = &fixture.services.threads;

	assert_eq!(threads.recount_thread_replies().await, 1);
	assert_eq!(room.count(&stale).await?, 1);
	assert_eq!(room.count(&kept).await?, 1);

	Ok(())
}

#[tokio::test]
async fn redacting_uncounted_replies_keeps_the_count() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let room = Room::new(&fixture.services).await?;
	let other = Room::create(&fixture.services, room_id!("!other:localhost")).await?;
	let [root, answer, backfilled, elsewhere] =
		["root", "answer", "backfilled", "elsewhere"].map(id);

	room.append(1, &root, text()).await?;
	room.append(2, &answer, thread(&root)).await?;
	room.store(PduCount::Backfilled(-1), &backfilled, thread(&root))?;
	other.append(3, &elsewhere, thread(&root)).await?;
	room.redact(&backfilled).await?;
	other.redact(&elsewhere).await?;

	assert_eq!(room.count(&root).await?, 1);

	Ok(())
}

#[tokio::test]
async fn redacted_root_keeps_its_thread_summary() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let room = Room::new(&fixture.services).await?;
	let [root, first, second, third, alone] =
		["root", "first", "second", "third", "alone"].map(id);

	room.append(1, &root, text()).await?;
	room.append(2, &first, thread(&root)).await?;
	room.append(3, &second, thread(&root)).await?;
	room.append(4, &alone, text()).await?;

	let summary = room.stored(&root).await?.1["unsigned"]["m.relations"]["m.thread"].clone();

	assert_eq!(
		(&summary["count"], &summary["latest_event"]["event_id"]),
		(&json!(2), &json!(second))
	);

	room.redact(&root).await?;
	room.redact(&alone).await?;

	let (_, redacted) = room.stored(&root).await?;

	assert_eq!(redacted["content"], json!({}));
	assert_eq!(redacted["unsigned"]["m.relations"], json!({ "m.thread": summary }));
	assert!(
		room.stored(&alone).await?.1["unsigned"]
			.get("m.relations")
			.is_none()
	);

	// Redacting a reply still counts down from the kept summary.
	room.redact(&first).await?;
	assert_eq!(room.count(&root).await?, 1);

	room.append(5, &third, thread(&root)).await?;

	assert_eq!(room.count(&root).await?, 2);

	Ok(())
}

#[tokio::test]
async fn redacted_self_rooted_reply_keeps_no_summary() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let room = Room::new(&fixture.services).await?;
	let looped = id("looped");

	room.append(1, &looped, thread(&looped)).await?;
	assert_eq!(room.count(&looped).await?, 1);

	room.redact(&looped).await?;

	let (_, redacted) = room.stored(&looped).await?;

	assert_eq!(redacted["content"], json!({}));
	assert!(redacted["unsigned"].get("m.relations").is_none());

	Ok(())
}

impl<'a> Room<'a> {
	async fn new(services: &'a Services) -> Result<Self> {
		Self::create(services, room_id!("!thread:localhost")).await
	}

	async fn create(services: &'a Services, room: &'static RoomId) -> Result<Self> {
		let (short, state) = (&services.short, &services.state);
		let create = &id(&format!("create-{}", room.strip_sigil().replace(':', "-")));
		let content = json!({ "creator": "@alice:localhost", "room_version": "10" });
		let mut pdu = event(room, create, "m.room.create", content);

		pdu["state_key"] = json!("");
		services.db["eventid_outlierpdu"].raw_put(create, Json(pdu));

		let key = short.get_or_create_shortstatekey(&StateEventType::RoomCreate, "");
		let compressed = services
			.state_compressor
			.compress_state_event(key.await, create);

		let state_hash = state.set_event_state(create, room, Arc::new([compressed.await].into()));

		state.set_room_state(room, state_hash.await?, &state.mutex.lock(room).await);

		Ok(Self(services, short.get_or_create_shortroomid(room).await, room))
	}

	/// Store an event and index its relation as `append_pdu_effects` does.
	async fn append(&self, count: u64, event_id: &EventId, content: Value) -> Result<RawPduId> {
		let (Self(services, ..), count) = (self, PduCount::Normal(count));
		let (timeline, threads) = (&services.timeline, &services.threads);
		let (pdu_id, pdu) = self.store(count, event_id, content)?;

		if let Some(target) = pdu.get_content_as_value()["m.relates_to"]["event_id"].as_str() {
			let target = timeline.get_pdu_count(target.try_into()?).await?;

			services.pdu_metadata.add_relation(count, target);
		}

		if let Some(root) = thread_root(pdu.get_content_as_value()) {
			threads.add_to_thread(&root, pdu_id, &pdu).await?;
		}

		Ok(pdu_id)
	}

	/// Store an event without indexing it, as backfill does.
	fn store(
		&self,
		count: PduCount,
		event_id: &EventId,
		content: Value,
	) -> Result<(RawPduId, PduEvent)> {
		let Self(services, shortroomid, room) = self;
		let pdu_id: RawPduId = PduId { shortroomid: *shortroomid, count }.into();
		let pdu: PduEvent =
			serde_json::from_value(event(room, event_id, "m.room.message", content))?;

		services.db["eventid_pduid"].insert(event_id.as_bytes(), pdu_id.as_bytes());
		services.db["pduid_pdu"].raw_put(pdu_id, Json(&pdu));

		Ok((pdu_id, pdu))
	}

	async fn redact(&self, target: &EventId) -> Result {
		let Self(services, shortroomid, room) = self;
		let content = json!({ "redacts": target });
		let reason = event(room, event_id!("$redaction:localhost"), "m.room.redaction", content);
		let reason: PduEvent = serde_json::from_value(reason)?;
		let lock = services.state.mutex.lock(*room).await;

		let redacted = services
			.timeline
			.redact_pdu(target, &reason, *shortroomid, &lock);

		redacted.await
	}

	async fn count(&self, root: &EventId) -> Result<Value> {
		Ok(self.stored(root).await?.1["unsigned"]["m.relations"]["m.thread"]["count"].clone())
	}

	/// Overwrite the root's count, as an earlier server version left it.
	async fn set_count(&self, root: &EventId, count: u64) -> Result {
		let (pdu_id, mut pdu) = self.stored(root).await?;

		pdu["unsigned"]["m.relations"]["m.thread"]["count"] = json!(count);
		self.0.db["pduid_pdu"].raw_put(pdu_id, Json(&pdu));

		Ok(())
	}

	async fn stored(&self, event_id: &EventId) -> Result<(RawPduId, Value)> {
		let pdu_id = self.0.timeline.get_pdu_id(event_id).await?;
		let pdu = self.0.timeline.get_pdu_json_from_id(&pdu_id);

		Ok((pdu_id, serde_json::to_value(pdu.await?)?))
	}
}

impl Wake for CountAtReplyWrite {
	fn wake(self: Arc<Self>) { self.wake_by_ref(); }

	fn wake_by_ref(self: &Arc<Self>) {
		let Self(services, root_id, count) = &**self;
		let root = services.db["pduid_pdu"].get_blocking(root_id);
		let root: Value = serde_json::from_slice(&root.expect("root")).expect("JSON");

		*count.lock().expect("locked") =
			root["unsigned"]["m.relations"]["m.thread"]["count"].clone();
	}
}

fn id(name: &str) -> OwnedEventId {
	format!("${name}:localhost")
		.try_into()
		.expect("test event ID")
}

fn event(room: &RoomId, event_id: &EventId, kind: &str, content: Value) -> Value {
	let mut event = json!({
		"type": kind, "event_id": event_id, "room_id": room, "sender": "@alice:localhost",
		"origin_server_ts": 1, "depth": 1, "hashes": { "sha256": "hash" },
		"prev_events": [], "auth_events": [],
	});

	event["content"] = content;
	event
}

fn text() -> Value { json!({ "msgtype": "m.text", "body": "text" }) }

fn thread(root: &EventId) -> Value { reply(root, "m.thread") }

fn legacy_reply(root: &EventId) -> Value { reply(root, "io.element.thread") }

fn reply(root: &EventId, rel_type: &str) -> Value {
	let relates_to = json!({ "rel_type": rel_type, "event_id": root });

	json!({ "msgtype": "m.text", "body": "reply", "m.relates_to": relates_to })
}

fn edit(target: &EventId) -> Value {
	json!({
		"msgtype": "m.text", "body": "* edit", "m.new_content": text(),
		"m.relates_to": { "rel_type": "m.replace", "event_id": target },
	})
}
