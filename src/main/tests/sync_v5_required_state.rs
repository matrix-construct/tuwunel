#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Result, ruma::RoomId};
use tuwunel_service::Services;

use self::{
	client::{Client, register, wait_until_ready},
	required_state::expect_state,
};

mod client;
mod sliding_sync;

#[path = "client/put.rs"]
mod put;

#[path = "sliding_sync/required_state.rs"]
mod required_state;

#[path = "sliding_sync/resume.rs"]
mod resume;

#[path = "client/state.rs"]
mod state;

const TOKEN: &str = "sync-v5-required-state-test-access-token";
const BEACON: &str = "org.matrix.msc3672.beacon_info";
const PINNED: &str = "m.room.pinned_events";

#[test]
#[tracing::instrument(level = "debug", skip_all)]
fn configuration_changes_only_deliver_newly_required_state() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let driven = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (served, outcome) = join(async_run(&server), driven).await;

		drop(services);
		async_stop(&server).await?;
		served?;

		outcome
	});

	drop(runtime);

	result
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let user = register(services, "requiredstate", TOKEN).await?;
	let client = Client { services, base, token: TOKEN };
	let room = client
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let beacon_id = client
		.put_state(&room, BEACON, user.as_str(), &json!({ "live": true }))
		.await?;

	let pinned_id = client
		.put_state(&room, PINNED, "", &json!({ "pinned": [] }))
		.await?;

	let listed = config(&room, 1, "*", false);
	let opened = config(&room, 20, "*", true);
	let initial = client.sync(None, &listed).await?;

	expect_state(&initial, &room, &[beacon_id.as_str()], "initial")?;

	let location_path = format!("rooms/{room}/send/org.matrix.msc3672.beacon/location");
	let location = json!({
		"m.relates_to": { "rel_type": "m.reference", "event_id": beacon_id },
		"org.matrix.msc3488.location": { "uri": "geo:0,0" },
	});

	client.put(&location_path, &location).await?;

	let ordinary = client.sync(Some(&initial), &listed).await?;

	expect_state(&ordinary, &room, &[], "location update")?;

	let raised = client
		.sync(Some(&ordinary), &config(&room, 20, "*", false))
		.await?;

	expect_state(&raised, &room, &[], "increased timeline limit")?;

	let lowered = client.sync(Some(&raised), &listed).await?;

	expect_state(&lowered, &room, &[], "decreased timeline limit")?;

	let subscribed = client.sync(Some(&lowered), &opened).await?;

	expect_state(&subscribed, &room, &[pinned_id.as_str()], "added subscription")?;

	let narrowed = client.sync(Some(&subscribed), &listed).await?;

	expect_state(&narrowed, &room, &[], "removed subscription")?;

	let pinned_id = client
		.put_state(&room, PINNED, "", &json!({ "pinned": [beacon_id] }))
		.await?;

	let consumed = client.sync(Some(&narrowed), &listed).await?;

	expect_state(&consumed, &room, &[], "unrequested pinned update")?;

	let reopened = client.sync(Some(&consumed), &opened).await?;

	expect_state(&reopened, &room, &[pinned_id.as_str()], "restored subscription")?;

	let replayed = client.sync(Some(&consumed), &opened).await?;

	expect_state(&replayed, &room, &[pinned_id.as_str()], "lost response replay")?;

	let stopped_id = client
		.put_state(&room, BEACON, user.as_str(), &json!({ "live": false }))
		.await?;

	let changed = client.sync(Some(&replayed), &opened).await?;

	expect_state(&changed, &room, &[stopped_id.as_str()], "changed beacon")?;

	let selected = config(&room, 20, user.as_str(), true);
	let exact = client.sync(Some(&changed), &selected).await?;

	expect_state(&exact, &room, &[], "wildcard narrowed to exact key")?;

	let other_id = client
		.put_state(&room, BEACON, "other", &json!({ "live": true }))
		.await?;

	let consumed = client.sync(Some(&exact), &selected).await?;

	expect_state(&consumed, &room, &[], "unrequested beacon key")?;

	let wildcard = client.sync(Some(&consumed), &opened).await?;

	expect_state(&wildcard, &room, &[other_id.as_str()], "exact key widened to wildcard")
}

fn config(room: &RoomId, limit: u64, key: &str, subscribed: bool) -> Value {
	let subscriptions = match subscribed {
		| false => json!({}),
		| true => {
			json!({ room.as_str(): { "required_state": [[PINNED, ""]], "timeline_limit": 20 } })
		},
	};

	json!({
		"conn_id": "required-state",
		"lists": { "all": {
			"ranges": [[0, 9]], "required_state": [[BEACON, key]], "timeline_limit": limit,
		} },
		"room_subscriptions": subscriptions,
	})
}
