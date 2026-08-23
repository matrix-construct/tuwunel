#![cfg(test)]

use std::{
	env::var, fs::remove_dir_all, net::TcpListener, path::PathBuf, process::id as process_id,
};

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Event, Result, ruma::events::TimelineEventType};
use tuwunel_service::Services;

use self::client::{Client, register, wait_until_ready};

mod client;

const ACCESS_TOKEN: &str = "messages-empty-filtered-page-access-token";
const EXCLUDED_FILTER: &str = r#"{"types":["com.example.never"]}"#;

/// Keeps exhausted backward pagination advancing to the local edge.
///
/// Removing the create event's timeline row models a partial local room whose
/// oldest surviving event can trigger backfill. The test covers a short local
/// tail, a fully filtered tail, and a failed backfill at that edge.
#[test]
fn exhausted_backward_page_uses_scanned_edge() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path =
		PathBuf::from(root).join(format!("tuwunel-messages-empty-page-{}", process_id()));

	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.option.extend([
		format!("database_path={db_path:?}"),
		"address=[\"127.0.0.1\"]".to_owned(),
		format!("port={port}"),
		"listening=true".to_owned(),
	]);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run_result, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	drop(runtime);
	remove_dir_all(&db_path).ok();

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;
	register(services, "messagesemptypage", ACCESS_TOKEN).await?;

	let client = Client { services, base, token: ACCESS_TOKEN };

	let room = client
		.create_room(&json!({
			"preset": "private_chat",
			"initial_state": [{
				"type": "m.room.history_visibility",
				"state_key": "",
				"content": {"history_visibility": "world_readable"},
			}],
		}))
		.await?;

	let (_, create) = services
		.timeline
		.first_item_in_room(&room)
		.await?;

	assert_eq!(*create.event_type(), TimelineEventType::RoomCreate);

	let create_pdu_id = services
		.timeline
		.get_pdu_id(&create.event_id)
		.await?;

	services
		.db
		.get("pduid_pdu")?
		.remove(&create_pdu_id);

	services.clear_cache().await;

	let (first_count, first_pdu) = services
		.timeline
		.first_item_in_room(&room)
		.await?;

	assert_ne!(*first_pdu.event_type(), TimelineEventType::RoomCreate);
	let expected_end = first_count.to_string();

	let tail = messages(&client, room.as_str(), &[("dir", "b"), ("limit", "100")]).await?;
	let chunk = tail["chunk"]
		.as_array()
		.expect("messages response chunk");

	assert!(chunk.first().is_some_and(Value::is_object));
	assert!(chunk.len() < 100);
	assert_eq!(tail["end"].as_str(), Some(expected_end.as_str()));

	let filtered_query = [("dir", "b"), ("limit", "100"), ("filter", EXCLUDED_FILTER)];

	let filtered = messages(&client, room.as_str(), &filtered_query).await?;

	assert!(
		filtered["chunk"]
			.as_array()
			.is_some_and(Vec::is_empty)
	);

	assert_eq!(filtered["end"].as_str(), Some(expected_end.as_str()));

	let at_edge_query = [("dir", "b"), ("limit", "100"), ("from", expected_end.as_str())];

	let at_edge = messages(&client, room.as_str(), &at_edge_query).await?;

	assert!(
		at_edge["chunk"]
			.as_array()
			.is_some_and(Vec::is_empty)
	);

	assert!(at_edge.get("end").is_none());

	Ok(())
}

async fn messages(client: &Client<'_>, room: &str, query: &[(&str, &str)]) -> Result<Value> {
	Ok(client
		.services
		.client
		.clients
		.default
		.get(client.url(&format!("rooms/{room}/messages")))
		.bearer_auth(client.token)
		.query(query)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?)
}
