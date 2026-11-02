//! Thread redaction keeps `m.thread` summaries and the thread list consistent.
//!
//! Redacting a thread reply takes it off its root's `m.thread.count`, and
//! redacting the last one takes the thread off the thread list. The recount
//! and the index rebuild repair stale counts and listings.

use std::{
	pin::pin,
	sync::{Arc, Mutex},
	task::{Wake, Waker},
};

use futures::{StreamExt, TryStreamExt};
use ruma::{
	EventId, OwnedEventId, RoomId, UserId, api::client::threads::get_threads::v1::IncludeThreads,
	event_id, events::StateEventType, room_id, user_id,
};
use serde_json::{Value, json, to_string, to_value};
use tuwunel_core::{
	Result,
	config::Figment,
	implement,
	utils::{BoolExt, ReadyExt, result::NotFound, stream::TryIgnore},
};
use tuwunel_database::Json;
use tuwunel_matrix::{
	Event,
	pdu::{PduCount, PduEvent, PduId, RawPduId},
};

use crate::{
	Services,
	rooms::{short::ShortRoomId, threads::thread_root},
	test_utils::{fixture, poll_once},
};

/// A room with just its create event, so redaction can read its version.
struct Room<'a>(&'a Services, ShortRoomId, &'static RoomId);

/// The root's stored count when the redacted reply's write is announced.
struct CountAtReplyWrite(Arc<Services>, RawPduId, Mutex<Value>);

/// The thread index rows naming one root.
///
/// Counts its activity rows, and records whether its validity pointer and its
/// participant row are stored.
#[derive(Debug, PartialEq)]
struct IndexRows {
	activity: usize,
	pointer: bool,
	participants: bool,
}

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

	// Redacting the last reply drops the summary, count included.
	let counts = [(&note, json!(2)), (&fix, json!(2)), (&first, json!(1)), (&first, json!(1))];

	for (event, count) in counts.into_iter().chain([(&legacy, Value::Null)]) {
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
	let [root, first, second] = ["root", "first", "second"].map(id);
	let root_id = room.append(1, &root, text()).await?;
	let first_id = room.append(2, &first, thread(&root)).await?;

	room.append(3, &second, thread(&root)).await?;

	let observed = Arc::new(CountAtReplyWrite(services.clone(), root_id, Mutex::default()));
	let waker = Waker::from(observed.clone());
	let mut watcher = pin!(services.db["pduid_pdu"].watch_raw_prefix_once(first_id));

	assert!(poll_once(watcher.as_mut(), &waker).is_pending());

	room.redact(&first).await?;

	assert_eq!(*observed.2.lock().expect("locked"), 1);

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

	assert_eq!(threads.rebuild_thread_summaries().await, (1, 0));
	assert_eq!(room.count(&stale).await?, 1);
	assert_eq!(room.count(&kept).await?, 1);

	// A second pass finds nothing left to change.
	assert_eq!(threads.rebuild_thread_summaries().await, (0, 0));

	Ok(())
}

#[tokio::test]
async fn rebuild_keeps_the_count_when_a_reply_cannot_be_decoded() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let room = Room::new(&fixture.services).await?;
	let [root, first, second] = ["root", "first", "second"].map(id);

	room.append(1, &root, text()).await?;
	let first_id = room.append(2, &first, thread(&root)).await?;

	room.append(3, &second, thread(&root)).await?;

	assert_eq!(room.count(&root).await?, 2);
	fixture.services.db["pduid_pdu"].insert(first_id.as_bytes(), b"invalid PDU");

	let threads = &fixture.services.threads;

	assert_eq!(threads.rebuild_thread_summaries().await, (0, 1));

	assert_eq!(room.count(&root).await?, 2);

	Ok(())
}

#[tokio::test]
async fn rebuild_drops_a_backfilled_roots_unloadable_latest() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let room = Room::new(&fixture.services).await?;
	let [root, gone] = ["root", "gone"].map(id);

	room.store(PduCount::Backfilled(-1), &root, text())?;
	room.append(1, &gone, thread(&root)).await?;
	fixture.services.db["eventid_pduid"].remove(gone.as_bytes());

	let threads = &fixture.services.threads;

	assert_eq!(threads.rebuild_thread_summaries().await, (1, 0));
	assert_eq!(room.count(&root).await?, Value::Null);
	assert_eq!(threads.rebuild_thread_summaries().await, (0, 0));

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
async fn redacting_every_reply_unlists_the_thread() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let room = Room::new(&fixture.services).await?;
	let [root, first, second, other, answer, live, reply, again] =
		["root", "first", "second", "other", "answer", "live", "reply", "again"].map(id);

	room.append(1, &root, text()).await?;
	room.append(2, &first, thread(&root)).await?;
	room.append(3, &second, thread(&root)).await?;
	room.append(4, &other, text()).await?;
	room.append(5, &answer, thread(&other)).await?;
	room.append(6, &live, text()).await?;
	room.append(7, &reply, thread(&live)).await?;

	assert_eq!(room.listed().await?, [&*live, &*other, &*root]);

	room.redact(&first).await?;
	assert_eq!(room.listed().await?, [&*live, &*other, &*root]);

	// Whichever goes last, the root or its reply, the emptied thread is unlisted.
	room.redact(&second).await?;
	room.redact(&root).await?;
	room.redact(&other).await?;
	assert_eq!(room.listed().await?, [&*live, &*other]);

	room.redact(&answer).await?;
	assert_eq!(room.listed().await?, [&*live]);

	let unlisted = IndexRows {
		activity: 0,
		pointer: false,
		participants: false,
	};

	for emptied in [&root, &other] {
		assert_eq!(room.index_rows(emptied).await?, unlisted, "{emptied}");
	}

	room.append(8, &again, thread(&root)).await?;

	let listed = IndexRows {
		activity: 1,
		pointer: true,
		participants: true,
	};

	assert_eq!(room.listed().await?, [&*root, &*live]);
	assert_eq!(room.index_rows(&root).await?, listed);

	Ok(())
}

#[implement(Room, params = "<'_>")]
async fn listed(&self) -> Result<Vec<OwnedEventId>> {
	let Self(services, _, room) = self;

	services
		.threads
		.threads_until(sender(), room, PduCount::max(), &IncludeThreads::All)
		.map_ok(|(_, pdu)| pdu.event_id)
		.try_collect()
		.await
}

#[implement(Room, params = "<'_>")]
async fn index_rows(&self, root: &EventId) -> Result<IndexRows> {
	let db = &self.0.db;
	let (root_id, _) = self.stored(root).await?;
	let activity = db["threadactivityid_rootid"]
		.raw_stream()
		.ignore_err()
		.ready_filter(|(_, value)| *value == root_id.as_bytes())
		.count()
		.await;

	let pointer = db["threadrootid_latestcount"]
		.exists(&root_id)
		.await
		.present()?
		.is_some();

	let participants = db["threadid_userids"]
		.exists(&root_id)
		.await
		.present()?
		.is_some();

	Ok(IndexRows { activity, pointer, participants })
}

#[tokio::test]
async fn rebuild_leaves_out_threads_with_no_replies() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let room = Room::new(&fixture.services).await?;
	let [emptied, reply, zero, last, kept, answer, old, late] =
		["emptied", "reply", "zero", "last", "kept", "answer", "old", "late"].map(id);

	let emptied_id = room.append(1, &emptied, text()).await?;
	let reply_id = room.append(2, &reply, thread(&emptied)).await?;
	let zero_id = room.append(3, &zero, text()).await?;
	let last_id = room.append(4, &last, thread(&zero)).await?;

	room.append(5, &kept, text()).await?;
	room.append(6, &answer, thread(&kept)).await?;
	room.store(PduCount::Backfilled(-1), &old, text())?;
	room.append(7, &late, thread(&old)).await?;

	// An earlier version kept these listed after redacting their last replies.
	room.redact(&reply).await?;
	room.relist(emptied_id, reply_id);
	room.redact(&last).await?;
	room.relist(zero_id, last_id);

	// A recount left this one a summary counting no reply.
	room.set_count(&zero, 0).await?;

	// A redacted root with a reply left keeps its summary, and its listing.
	room.redact(&kept).await?;

	assert_eq!(room.listed().await?, [&*old, &*kept, &*zero, &*emptied]);

	fixture
		.services
		.threads
		.rebuild_thread_activity()
		.await?;

	// The backfilled root's reply is not in the relation index; its summary lists it.
	assert_eq!(room.listed().await?, [&*kept, &*old]);

	Ok(())
}

#[implement(Room, params = "<'_>")]
fn relist(&self, root_id: RawPduId, reply_id: RawPduId) {
	let db = &self.0.db;
	let latest = reply_id.pdu_count().to_be_bytes();

	db["threadid_userids"].insert(&root_id, sender().as_str());
	db["threadactivityid_rootid"].insert(&reply_id, root_id);
	db["threadrootid_latestcount"].insert(&root_id, latest);
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

#[tokio::test]
async fn redacting_the_latest_reply_shows_the_newest_remaining_one() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = Room::new(services).await?;
	let [root, first, second, third] = ["root", "first", "second", "third"].map(id);
	let mut secret = thread(&root);

	secret["body"] = json!("secret");
	room.append(1, &root, text()).await?;
	room.append(2, &first, thread(&root)).await?;
	room.append(3, &second, thread(&root)).await?;
	room.append(4, &third, secret).await?;
	room.redact(&first).await?;

	assert_eq!(room.latest(&root).await?, json!(third));

	room.redact(&third).await?;

	let served = services.timeline.get_pdu(&root).await?;
	let served = services
		.pdu_metadata
		.bundle_aggregations(sender(), served);

	assert!(!to_string(&served.await)?.contains("secret"));
	assert_eq!(room.latest(&root).await?, json!(second));

	room.redact(&second).await?;

	// With no reply left, the summary goes, as on Synapse.
	assert!(
		room.stored(&root).await?.1["unsigned"]
			.get("m.relations")
			.is_none()
	);

	Ok(())
}

#[tokio::test]
async fn replacement_error_omits_the_redacted_latest_and_decrements_count() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = Room::new(services).await?;
	let [root, older, faulty, newest] = ["root", "older", "faulty", "newest"].map(id);
	let secret = json!({
		"msgtype": "m.text", "body": "redacted thread preview secret",
		"m.relates_to": { "rel_type": "m.thread", "event_id": root },
	});

	room.append(1, &root, text()).await?;
	room.append(2, &older, thread(&root)).await?;
	let faulty_id = room.append(3, &faulty, thread(&root)).await?;

	room.append(4, &newest, secret).await?;
	assert_eq!(room.latest(&root).await?, json!(newest));
	assert_eq!(room.count(&root).await?, json!(3));
	services.db["pduid_pdu"].insert(faulty_id.as_bytes(), b"invalid PDU");

	let error = services
		.timeline
		.get_pdu_from_id(&faulty_id)
		.await
		.expect_err("replacement child must fail decoding");

	assert!(error.is_not_found().is_false());
	room.redact(&newest).await?;

	let (_, redacted) = room.stored(&newest).await?;

	assert_eq!(redacted["content"], json!({}));
	assert!(
		redacted["unsigned"]
			.get("redacted_because")
			.is_some()
	);

	let served = services.timeline.get_pdu(&root).await?;
	let served = services
		.pdu_metadata
		.bundle_aggregations(sender(), served)
		.await;

	assert!(
		to_string(&served)?
			.contains("redacted thread preview secret")
			.is_false()
	);

	assert!(
		to_value(served)?["unsigned"]
			.get("m.relations")
			.is_none()
	);

	assert_eq!(room.latest(&root).await?, Value::Null);
	assert_eq!(room.count(&root).await?, json!(2));

	Ok(())
}

#[tokio::test]
async fn replacement_search_counts_missing_relation_rows() -> Result {
	let config = Figment::new().merge(("rooms.redaction.thread_latest_reply_search_limit", 3));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let room = Room::new(&fixture.services).await?;
	let [root, old, newest, later] = ["root", "old", "newest", "later"].map(id);

	room.append(1, &root, text()).await?;
	room.append(2, &old, thread(&root)).await?;
	fixture
		.services
		.pdu_metadata
		.add_relation(PduCount::Normal(3), PduCount::Normal(1));

	fixture
		.services
		.pdu_metadata
		.add_relation(PduCount::Normal(4), PduCount::Normal(1));

	room.append(5, &newest, thread(&root)).await?;
	let before = room.stored(&root).await?.1["unsigned"]["m.relations"]["m.thread"]
		["current_user_participated"]
		.clone();

	room.redact(&newest).await?;

	assert_eq!(room.latest(&root).await?, Value::Null);
	assert_eq!(room.count(&root).await?, json!(1));
	assert_eq!(
		room.stored(&root).await?.1["unsigned"]["m.relations"]["m.thread"]
			["current_user_participated"],
		before
	);

	let served = fixture.services.timeline.get_pdu(&root).await?;
	let served = fixture
		.services
		.pdu_metadata
		.bundle_aggregations(sender(), served)
		.await;

	assert!(
		to_value(served)?["unsigned"]
			.get("m.relations")
			.is_none()
	);

	room.append(6, &later, thread(&root)).await?;

	assert_eq!(room.latest(&root).await?, json!(later));
	assert_eq!(room.count(&root).await?, json!(2));

	room.redact(&later).await?;
	room.redact(&old).await?;

	assert!(
		room.stored(&root).await?.1["unsigned"]
			.get("m.relations")
			.is_none()
	);

	Ok(())
}

#[tokio::test]
async fn replacement_search_keeps_a_reply_at_the_limit() -> Result {
	let config = Figment::new().merge(("rooms.redaction.thread_latest_reply_search_limit", 3));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let room = Room::new(&fixture.services).await?;
	let [root, old, newest] = ["root", "old", "newest"].map(id);

	room.append(1, &root, text()).await?;
	room.append(2, &old, thread(&root)).await?;
	fixture
		.services
		.pdu_metadata
		.add_relation(PduCount::Normal(3), PduCount::Normal(1));

	room.append(4, &newest, thread(&root)).await?;

	room.redact(&newest).await?;

	assert_eq!(room.latest(&root).await?, json!(old));
	assert_eq!(room.count(&root).await?, json!(1));

	Ok(())
}

impl<'a> Room<'a> {
	async fn new(services: &'a Services) -> Result<Self> {
		Self::create(services, room_id!("!thread:localhost")).await
	}

	async fn create(services: &'a Services, room: &'static RoomId) -> Result<Self> {
		let (short, state) = (&services.short, &services.state);
		let create = &id(&format!("create-{}", room.strip_sigil().replace(':', "-")));
		let content = json!({ "creator": sender(), "room_version": "10" });
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

		if let Some(root) = thread_root(&pdu.get_content_as_value().try_into()?) {
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

	async fn latest(&self, root: &EventId) -> Result<Value> {
		let thread = &self.stored(root).await?.1["unsigned"]["m.relations"]["m.thread"];

		Ok(thread["latest_event"]["event_id"].clone())
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

		Ok((pdu_id, to_value(pdu.await?)?))
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
		"type": kind, "event_id": event_id, "room_id": room, "sender": sender(),
		"origin_server_ts": 1, "depth": 1, "hashes": { "sha256": "hash" },
		"prev_events": [], "auth_events": [],
	});

	event["content"] = content;
	event
}

fn sender() -> &'static UserId { user_id!("@alice:localhost") }

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
