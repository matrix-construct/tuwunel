use ruma::server_name;
use serde_json::{Value, json};
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::Json;

use super::{fresh, local_user_id, marker_present, migrate};
use crate::{
	Services,
	test_utils::{fixture, pdu_id},
};

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
async fn thread_reply_recount_runs_once() -> Result {
	let config = Figment::new().merge(("create_admin_room", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let marker = "recount_thread_replies";

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

fn set_root_count(services: &Services, count: u64) {
	let thread = json!({ "m.relations": { "m.thread": { "count": count } } });
	let root = json!({ "room_id": "!thread:localhost", "unsigned": thread });

	services.db["pduid_pdu"].raw_put(pdu_id(1), Json(root));
}

async fn root_count(services: &Services) -> Result<Value> {
	let root = services
		.timeline
		.get_pdu_json_from_id(&pdu_id(1))
		.await?;

	Ok(serde_json::to_value(root)?["unsigned"]["m.relations"]["m.thread"]["count"].clone())
}
