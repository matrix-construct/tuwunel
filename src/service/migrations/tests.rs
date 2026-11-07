use futures::{Stream, TryStreamExt};
use ruma::{MilliSecondsSinceUnixEpoch, owned_user_id, server_name, uint, user_id};
use serde_json::{Value, json};
use tuwunel_core::{Result, config::Figment, itertools::Itertools, result::NotFound};
use tuwunel_database::{Json, serialize_key};
use tuwunel_matrix::pdu::PduCount;

use super::{
	MARKERS, REBUILD_LIVE_THREAD_ACTIVITY, REBUILD_THREAD_SUMMARIES, RERUNS, Stamp, fresh,
	local_user_id, marker_present, migrate, migrate_profile_keys::repair as repair_profile_value,
	moderation::copy as copy_moderation, rerun, rerun_markers, tally::Outcome,
};
use crate::{
	Services,
	integrity::{
		Report, checks,
		moderation::{Source, scan as moderation},
		useridprofilekey_value::Finding,
	},
	test_utils::{fixture, pdu_id},
	users::Moderation,
};

mod adoption;

#[test]
fn localpart_becomes_a_local_user_id() {
	let user_id = local_user_id("alice", server_name!("example.org"))
		.expect("a plain localpart composes a user id");

	assert_eq!(user_id.as_str(), "@alice:example.org");
}

#[test]
fn localpart_past_the_inline_budget_survives() {
	let localpart = "a".repeat(64);
	let user_id = local_user_id(&localpart, server_name!("example.org"))
		.expect("a spilled buffer still composes a user id");

	assert_eq!(user_id.localpart(), localpart);
}

#[test]
fn unusable_localpart_is_skipped() {
	assert!(local_user_id("alice\0bob", server_name!("example.org")).is_none());
}

#[tokio::test]
async fn profile_value_repair_reads_the_row_again() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let values = &services.db["useridprofilekey_value"];
	let key = serialize_key((user_id!("@alice:localhost"), "field"))?;
	let stale = Finding::Bare { key: key.clone(), text: "stale".into() };

	// The value was fixed after the scan reported it bare.
	fresh(services).await?;
	values.raw_put(&key, Json("current"));

	assert_eq!(repair_profile_value(values, &stale).await?, Outcome::Stale);
	assert_eq!(&*values.get(&key).await?, br#""current""#);

	// The row was removed after the scan reported it.
	values.remove(&key);

	assert_eq!(repair_profile_value(values, &stale).await?, Outcome::Stale);
	assert!(values.get(&key).await.is_not_found());

	Ok(())
}

#[tokio::test]
async fn moderation_copy_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let suspended = &services.db["userid_suspended"];
	let entry = |suspended: bool| {
		let entry = json!({ "suspended": suspended, "suspended_at": 1, "suspended_by": "@mod:localhost" });

		Json(entry)
	};

	let copy = Moderation {
		when: MilliSecondsSinceUnixEpoch(uint!(1)),
		by: owned_user_id!("@mod:localhost"),
	};

	// No service-level test can create a foreign column, so a native one stands in.
	let foreign = &services.db["userid_blurhash"];
	let source = Source {
		name: "userid_suspension",
		map: foreign.clone(),
		target: "userid_suspended",
	};

	fresh(services).await?;
	foreign.raw_put("@alice:localhost", entry(true));
	foreign.raw_put("@bob:localhost", entry(false));
	foreign.insert("@carol:localhost", "not json");
	foreign.raw_put("@dave:localhost", entry(true));
	foreign.raw_put("@erin:localhost", entry(true));
	suspended.raw_put("@dave:localhost", Json(json!({ "when": 2 })));
	suspended.raw_put("@erin:localhost", Json(&copy));

	assert_eq!(kinds(moderation(services, &source)).await?, [
		"uncopied-entry",
		"uncopied-entry",
		"unparsable-entry"
	]);

	copy_moderation(services, &source).await?;

	let copied = serde_json::to_vec(&copy)?;

	assert_eq!(kinds(moderation(services, &source)).await?, ["unparsable-entry"]);
	assert_eq!(&*suspended.get("@alice:localhost").await?, copied);
	assert_eq!(&*suspended.get("@dave:localhost").await?, copied);
	assert!(
		suspended
			.get("@bob:localhost")
			.await
			.is_not_found()
	);

	Ok(())
}

/// The kinds of finding a check's stream yields, sorted.
///
/// A walk that fans out yields its findings in no fixed order.
async fn kinds<S, F>(findings: S) -> Result<Vec<&'static str>>
where
	S: Stream<Item = Result<F>>,
	F: Report,
{
	findings
		.map_ok(|finding| finding.kind())
		.try_collect()
		.await
		.map(|kinds: Vec<_>| kinds.into_iter().sorted().collect())
}

#[tokio::test]
async fn thread_reply_recount_runs_once() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let marker = "rebuild_thread_summaries";

	// A new database has no counts to correct.
	fresh(services).await?;

	assert!(marker_present(services, marker).await?);

	// A root still counting three replies redacted earlier.
	set_root_count(services, 3);
	services.db["threadid_userids"].insert(&pdu_id(1), "@alice:localhost");
	services.db["global"].remove(marker);
	migrate(services, false).await?;

	assert_eq!(root_count(services).await?, 0);

	set_root_count(services, 3);
	migrate(services, false).await?;

	assert_eq!(root_count(services).await?, 3);

	Ok(())
}

#[tokio::test]
async fn redacted_thread_latest_scrub_runs_once() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let marker = "rebuild_thread_summaries";

	fresh(services).await?;

	assert!(marker_present(services, marker).await?);

	// The root's bundle still copies its only reply, redacted earlier.
	let gone = json!({
		"type": "m.room.message", "event_id": "$gone:localhost", "room_id": "!thread:localhost",
		"sender": "@alice:localhost", "origin_server_ts": 1, "depth": 1, "hashes": { "sha256": "hash" },
		"prev_events": [], "auth_events": [], "content": {}, "unsigned": { "redacted_because": {} },
	});

	services.db["eventid_pduid"].insert(b"$gone:localhost", pdu_id(2).as_bytes());
	services.db["pduid_pdu"].raw_put(pdu_id(2), Json(&gone));
	services.db["threadid_userids"].insert(&pdu_id(1), "@alice:localhost");
	services.db["global"].remove(marker);

	for (run, latest) in [(1, Value::Null), (2, json!("$gone:localhost"))] {
		set_root_latest(services, &gone);
		migrate(services, false).await?;

		assert_eq!(root_thread(services).await?["latest_event"]["event_id"], latest, "run {run}");
	}

	// A latest reply that can no longer be loaded is scrubbed too.
	services.db["global"].remove(marker);
	set_root_latest(services, &json!({ "event_id": "$purged:localhost" }));
	migrate(services, false).await?;

	assert_eq!(root_thread(services).await?, Value::Null);

	Ok(())
}

#[tokio::test]
async fn emptied_thread_unlisting_runs_once() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let markers = ["rebuild_thread_summaries", REBUILD_LIVE_THREAD_ACTIVITY];

	fresh(services).await?;

	assert!(marker_present(services, REBUILD_LIVE_THREAD_ACTIVITY).await?);

	// A summary the summary pass drops: its only reply is gone.
	set_root_latest(services, &json!({ "event_id": "$purged:localhost" }));

	for marker in markers {
		services.db["global"].remove(marker);
	}

	for (run, listed) in [(1, false), (2, true)] {
		list_root(services);
		migrate(services, false).await?;

		let pointer = services.db["threadrootid_latestcount"]
			.exists(&pdu_id(1))
			.await;

		assert_eq!(pointer.is_ok(), listed, "run {run}");
	}

	Ok(())
}

#[test]
fn rerun_table_covers_every_check() {
	let listed = RERUNS
		.iter()
		.map(|&(name, _)| name)
		.sorted_unstable();

	let registered = checks().map(|check| check.name).sorted_unstable();

	assert!(listed.eq(registered), "every check needs exactly one entry in the rerun table");
}

#[test]
fn rerun_unstamps_thread_activity_before_summaries() -> Result {
	// A summary rebuild scheduled alone can empty a thread its activity index still lists.
	let markers = rerun_markers("thread-summaries")?;

	assert_eq!(markers, [REBUILD_LIVE_THREAD_ACTIVITY, REBUILD_THREAD_SUMMARIES]);

	Ok(())
}

#[tokio::test]
async fn rerun_schedules_the_summary_and_activity_rebuilds() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let [summaries, activity] = [REBUILD_THREAD_SUMMARIES, REBUILD_LIVE_THREAD_ACTIVITY];

	fresh(services).await?;

	// A summary the summary pass drops, still listed by the activity index.
	set_root_latest(services, &json!({ "event_id": "$purged:localhost" }));
	list_root(services);

	let stamps = rerun_stamps(services, "thread-summaries").await?;

	assert_eq!(stamps, [(activity, true), (summaries, true)]);

	let stamps = rerun_stamps(services, "thread-summaries").await?;

	assert_eq!(stamps, [(activity, false), (summaries, false)]);

	migrate(services, false).await?;

	let pointer = services.db["threadrootid_latestcount"]
		.exists(&pdu_id(1))
		.await;

	assert!(pointer.is_err(), "the next start must unlist the emptied thread");
	assert!(marker_present(services, summaries).await?);
	assert!(marker_present(services, activity).await?);

	Ok(())
}

async fn rerun_stamps(services: &Services, check: &str) -> Result<Vec<(&'static str, bool)>> {
	rerun(services, check).await.map(|stamps| {
		stamps
			.map(|Stamp { marker, cleared }| (marker, cleared))
			.collect()
	})
}

#[tokio::test]
async fn rerun_refuses_a_check_without_a_safe_step() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let refusals = [
		("short-ids", "the outcome it records"),
		("schema", "it has no repair"),
		("legacy-media-names", "can lose media files"),
		("unrecorded-redactions", "redactions this server refused"),
		("foreign-token-expiry", "adopts a foreign database's state once"),
		("no-such-check", "no integrity check is named"),
	];

	fresh(services).await?;

	for (check, reason) in refusals {
		let error = rerun(services, check)
			.await
			.err()
			.map(|error| error.to_string());

		assert!(
			error
				.as_deref()
				.is_some_and(|error| error.contains(reason)),
			"{check} must be refused for {reason:?}, not {error:?}"
		);
	}

	for marker in MARKERS {
		assert!(marker_present(services, marker).await?, "{marker} must stay stamped");
	}

	Ok(())
}

fn list_root(services: &Services) {
	let latest = PduCount::Normal(2).to_be_bytes();

	services.db["threadid_userids"].insert(&pdu_id(1), "@alice:localhost");
	services.db["threadactivityid_rootid"].insert(&pdu_id(2), pdu_id(1));
	services.db["threadrootid_latestcount"].insert(&pdu_id(1), latest);
}

fn set_root_latest(services: &Services, latest: &Value) {
	let thread = json!({ "m.relations": { "m.thread": { "count": 1, "latest_event": latest } } });
	let root = json!({ "room_id": "!thread:localhost", "unsigned": thread });

	services.db["pduid_pdu"].raw_put(pdu_id(1), Json(root));
}

fn set_root_count(services: &Services, count: u64) {
	let thread = json!({ "m.relations": { "m.thread": { "count": count } } });
	let root = json!({ "room_id": "!thread:localhost", "unsigned": thread });

	services.db["pduid_pdu"].raw_put(pdu_id(1), Json(root));
}

async fn root_count(services: &Services) -> Result<Value> {
	Ok(root_thread(services).await?["count"].clone())
}

async fn root_thread(services: &Services) -> Result<Value> {
	let root = services
		.timeline
		.get_pdu_json_from_id(&pdu_id(1))
		.await?;

	Ok(serde_json::to_value(root)?["unsigned"]["m.relations"]["m.thread"].clone())
}
