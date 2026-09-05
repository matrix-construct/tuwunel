#![cfg(test)]

use std::{env::temp_dir, fs::remove_dir_all, net::TcpListener, process::id as process_id};

use futures::future::join;
use reqwest::{Response, StatusCode};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	PduId, Result,
	matrix::pdu::{PduCount, RawPduId},
	ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId},
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const ACCESS_TOKEN: &str = "messages-pagination-bounds-test-access-token";
const MESSAGE: &str = "m.room.message";
const MESSAGE_FILTER: &str = r#"{"types":["m.room.message"]}"#;
const EMPTY_FILTER: &str = r#"{"types":["com.example.never"]}"#;

#[test]
fn messages_respect_pagination_bounds() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let db_path = temp_dir().join(format!("tuwunel-pagination-bounds-{}", process_id()));
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
	register(services, "pagination", ACCESS_TOKEN).await?;

	invalid_tokens(services, base).await?;
	global_bounds(services, base).await?;
	filtered_bounds(services, base).await?;
	backfilled_bounds(services, base).await
}

async fn fixture<'a>(services: &'a Services, base: &'a str) -> Result<(Client<'a>, OwnedRoomId)> {
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

	Ok((client, room))
}

async fn global_bounds(services: &Services, base: &str) -> Result {
	let (client, room) = fixture(services, base).await?;
	let other = client
		.create_room(&json!({"preset": "private_chat"}))
		.await?;
	let first = send(&client, &room, MESSAGE, "first").await?;

	send(&client, &other, MESSAGE, "gap-one").await?;
	let low = sync_token(&client).await?;
	let middle = send(&client, &room, MESSAGE, "middle").await?;

	send(&client, &other, MESSAGE, "gap-two").await?;
	let high = sync_token(&client).await?;
	let last = send(&client, &room, MESSAGE, "last").await?;

	assert!(first.1.parse::<i64>()? < low.parse::<i64>()?);
	assert!(low.parse::<i64>()? < middle.1.parse::<i64>()?);
	assert!(middle.1.parse::<i64>()? < high.parse::<i64>()?);
	assert!(high.parse::<i64>()? < last.1.parse::<i64>()?);

	// A sync position can fall between this room's events. Neither direction
	// may continue until it happens to find an event with that exact count.
	for (dir, from, to) in [
		("b", &last.1, &low),
		("f", &first.1, &high),
		("b", &high, &low),
		("f", &low, &high),
	] {
		let page = page(&client, &room, &[
			("dir", dir),
			("from", from),
			("to", to),
			("filter", MESSAGE_FILTER),
			("limit", "100"),
		])
		.await?;

		assert_page(&page, &[middle.0.as_str()], Some(&middle.1));
	}

	// Exact, equal, and reversed bounds all exclude the boundary itself.
	for (dir, from, to) in [
		("b", &last.1, &middle.1),
		("f", &first.1, &middle.1),
		("b", &middle.1, &middle.1),
		("f", &middle.1, &middle.1),
		("b", &first.1, &high),
		("f", &last.1, &low),
	] {
		let page = page(&client, &room, &[
			("dir", dir),
			("from", from),
			("to", to),
			("filter", MESSAGE_FILTER),
		])
		.await?;

		assert_page(&page, &[], None);
	}

	Ok(())
}

async fn filtered_bounds(services: &Services, base: &str) -> Result {
	let (client, room) = fixture(services, base).await?;
	let other = client
		.create_room(&json!({"preset": "private_chat"}))
		.await?;

	send(&client, &room, MESSAGE, "before-bound").await?;
	send(&client, &other, MESSAGE, "low-gap").await?;
	let low = sync_token(&client).await?;

	send(&client, &room, "com.example.pagination", "filtered-event").await?;
	let visible = send(&client, &room, MESSAGE, "visible-event").await?;

	send(&client, &other, MESSAGE, "high-gap").await?;
	let high = sync_token(&client).await?;
	send(&client, &room, MESSAGE, "after-bound").await?;

	for (dir, from, to) in [("b", &high, &low), ("f", &low, &high)] {
		let partial = page(&client, &room, &[
			("dir", dir),
			("from", from),
			("to", to),
			("filter", MESSAGE_FILTER),
			("limit", "100"),
		])
		.await?;

		assert_page(&partial, &[visible.0.as_str()], Some(&visible.1));

		let empty = page(&client, &room, &[
			("dir", dir),
			("from", from),
			("to", to),
			("filter", EMPTY_FILTER),
			("limit", "100"),
		])
		.await?;

		assert_page(&empty, &[], None);
	}

	let limited = page(&client, &room, &[
		("dir", "b"),
		("from", &high),
		("to", &low),
		("filter", MESSAGE_FILTER),
		("limit", "1"),
	])
	.await?;

	assert_page(&limited, &[visible.0.as_str()], Some(&visible.1));

	let remainder = page(&client, &room, &[
		("dir", "b"),
		("from", field(&limited, "end")?),
		("to", &low),
		("filter", MESSAGE_FILTER),
		("limit", "1"),
	])
	.await?;

	assert_page(&remainder, &[], None);

	Ok(())
}

async fn invalid_tokens(services: &Services, base: &str) -> Result {
	let (client, room) = fixture(services, base).await?;
	let mut failures = Vec::new();

	for dir in ["b", "f"] {
		for name in ["from", "to"] {
			// Ruma's compatibility parser treats empty optional tokens as omitted.
			let response = messages(&client, &room, &[("dir", dir), (name, "")]).await?;

			assert_eq!(response.status(), StatusCode::OK);

			for token in
				[" ", "not-a-token", "1.5", "9223372036854775808", "-9223372036854775809"]
			{
				let response = messages(&client, &room, &[("dir", dir), (name, token)]).await?;
				let status = response.status();
				let body: Value = response.json().await?;

				if status != StatusCode::BAD_REQUEST || body["errcode"] != "M_INVALID_PARAM" {
					failures.push((dir, name, token, status, body["errcode"].clone()));
				}
			}
		}
	}

	assert!(
		failures.is_empty(),
		"malformed tokens must return 400 M_INVALID_PARAM: {failures:?}"
	);

	Ok(())
}

async fn backfilled_bounds(services: &Services, base: &str) -> Result {
	let (client, room) = fixture(services, base).await?;
	let first = send(&client, &room, MESSAGE, "oldest").await?;
	let middle = send(&client, &room, MESSAGE, "older").await?;
	let last = send(&client, &room, MESSAGE, "old").await?;

	for (event, count) in [(&first.0, -50), (&middle.0, -30), (&last.0, -10)] {
		move_to_backfilled_position(services, &room, event, count).await?;
	}

	services.clear_cache().await;

	let backward = page(&client, &room, &[
		("dir", "b"),
		("from", "0"),
		("to", "-40"),
		("filter", MESSAGE_FILTER),
	])
	.await?;

	assert_page(&backward, &[last.0.as_str(), middle.0.as_str()], Some("-30"));

	let forward = page(&client, &room, &[
		("dir", "f"),
		("from", "-60"),
		("to", "-20"),
		("filter", MESSAGE_FILTER),
	])
	.await?;

	assert_page(&forward, &[first.0.as_str(), middle.0.as_str()], Some("-30"));

	for (dir, from, expected, end) in
		[("b", "0", last.0.as_str(), "-10"), ("f", "-60", first.0.as_str(), "-50")]
	{
		let page = page(&client, &room, &[
			("dir", dir),
			("from", from),
			("to", "-30"),
			("filter", MESSAGE_FILTER),
		])
		.await?;

		assert_page(&page, &[expected], Some(end));
	}

	Ok(())
}

/// Re-key validated events to exercise stored backfill ordering without a
/// remote server. Pagination reads these timeline rows and event-id mappings.
async fn move_to_backfilled_position(
	services: &Services,
	room: &RoomId,
	event: &EventId,
	count: i64,
) -> Result {
	let old = services.timeline.get_pdu_id(event).await?;
	let new: RawPduId = PduId {
		shortroomid: services.short.get_shortroomid(room).await?,
		count: PduCount::Backfilled(count),
	}
	.into();
	let timeline = services.db.get("pduid_pdu")?;
	let stored = timeline.get(&old).await?;

	timeline.insert(&new, stored.as_ref());
	timeline.remove(&old);
	services
		.db
		.get("eventid_pduid")?
		.insert(event.as_bytes(), new);

	Ok(())
}

async fn send(
	client: &Client<'_>,
	room: &RoomId,
	kind: &str,
	txn: &str,
) -> Result<(OwnedEventId, String)> {
	let response: Value = client
		.services
		.client
		.clients
		.default
		.put(client.url(&format!("rooms/{room}/send/{kind}/{txn}")))
		.bearer_auth(client.token)
		.json(&json!({"msgtype": "m.text", "body": txn}))
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;
	let event: OwnedEventId = field(&response, "event_id")?.try_into()?;
	let count = client
		.services
		.timeline
		.get_pdu_id(&event)
		.await?
		.pdu_count();

	Ok((event, count.to_string()))
}

async fn sync_token(client: &Client<'_>) -> Result<String> {
	let response: Value = client
		.services
		.client
		.clients
		.default
		.get(client.url("sync"))
		.bearer_auth(client.token)
		.query(&[("timeout", "0")])
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(field(&response, "next_batch")?.to_owned())
}

async fn messages(
	client: &Client<'_>,
	room: &RoomId,
	query: &[(&str, &str)],
) -> Result<Response> {
	Ok(client
		.services
		.client
		.clients
		.default
		.get(client.url(&format!("rooms/{room}/messages")))
		.bearer_auth(client.token)
		.query(query)
		.send()
		.await?)
}

async fn page(client: &Client<'_>, room: &RoomId, query: &[(&str, &str)]) -> Result<Value> {
	Ok(messages(client, room, query)
		.await?
		.error_for_status()?
		.json()
		.await?)
}

fn assert_page(page: &Value, expected: &[&str], end: Option<&str>) {
	let events: Vec<_> = page["chunk"]
		.as_array()
		.expect("messages response chunk")
		.iter()
		.map(|event| event["event_id"].as_str().expect("event id"))
		.collect();

	assert_eq!(events, expected, "{page}");
	assert_eq!(page.get("end").and_then(Value::as_str), end, "{page}");
}
