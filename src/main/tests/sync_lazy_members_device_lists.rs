#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{OwnedEventId, RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const READER_TOKEN: &str = "sync-lazy-members-device-lists-reader-token";
const SENDER_TOKEN: &str = "sync-lazy-members-device-lists-sender-token";

/// Redundant members keep the sender's membership in every round's state.
const LAZY_FILTER: &str =
	r#"{"room":{"state":{"lazy_load_members":true,"include_redundant_members":true}}}"#;

/// Drives a lazily loaded plaintext room over the client API.
///
/// A member who speaks has their membership lazily loaded into the state
/// section of the round, which is display state rather than a membership
/// change, so the reader's `device_lists.changed` must leave them out. A
/// device-key change by the same member in the same room still lands there.
#[test]
fn lazy_members_are_not_device_list_changes() -> Result {
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

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	register(services, "lazymembersreader", READER_TOKEN).await?;
	let sender_id = register(services, "lazymemberssender", SENDER_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let sender = Client { services, base, token: SENDER_TOKEN };
	let room = sender
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	reader.join(&room).await?;

	let opening = reader.sync(None).await?;
	let since = next_batch(&opening)?;
	let body = json!({ "msgtype": "m.text", "body": "spoken" });

	sender
		.send(&room, "m.room.message", "spoken", &body)
		.await?;

	let spoken = reader.sync(Some(since)).await?;
	let since = next_batch(&spoken)?;

	lazily_loaded(&spoken, &room, &sender_id)
		.ok_or_else(|| err!("the spoken round did not lazily load the sender's membership"))?;

	device_list_changed(&spoken, &sender_id)
		.is_false()
		.ok_or_else(|| err!("the spoken round reported the lazily loaded sender as changed"))?;

	services
		.users
		.mark_device_key_update(&sender_id)
		.await;

	let rekeyed = reader.sync(Some(since)).await?;

	device_list_changed(&rekeyed, &sender_id)
		.ok_or_else(|| err!("the rekeyed round withheld the sender's device-list change"))?;

	Ok(())
}

#[implement(Client, params = "<'_>")]
async fn join(&self, room_id: &RoomId) -> Result {
	self.services
		.client
		.clients
		.default
		.post(self.url(&format!("rooms/{room_id}/join")))
		.bearer_auth(self.token)
		.json(&json!({}))
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

/// One non-blocking lazily loaded sync, optionally resuming from a token.
///
/// Every round is driven with `timeout=0` and the lazy-loading filter, so
/// the state section carries the witnessed members the case inspects.
#[implement(Client, params = "<'_>")]
async fn sync(&self, since: Option<&str>) -> Result<Value> {
	let since = since.map(|since| ("since", since));

	self.services
		.client
		.clients
		.default
		.get(self.url("sync"))
		.bearer_auth(self.token)
		.query(&[("timeout", "0"), ("filter", LAZY_FILTER)])
		.query(since.as_slice())
		.send()
		.await?
		.error_for_status()?
		.json()
		.await
		.map_err(Into::into)
}

fn next_batch(response: &Value) -> Result<&str> { field(response, "next_batch") }

#[implement(Client, params = "<'_>")]
async fn send(
	&self,
	room_id: &RoomId,
	event_type: &str,
	txn_id: &str,
	content: &Value,
) -> Result<OwnedEventId> {
	let path = format!("rooms/{room_id}/send/{event_type}/lazy-members-{txn_id}");
	let response: Value = self
		.services
		.client
		.clients
		.default
		.put(self.url(&path))
		.bearer_auth(self.token)
		.json(content)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	field(&response, "event_id")?
		.try_into()
		.map_err(Into::into)
}

fn lazily_loaded(response: &Value, room_id: &RoomId, user_id: &UserId) -> bool {
	response["rooms"]["join"][room_id.as_str()]["state"]["events"]
		.as_array()
		.into_iter()
		.flatten()
		.any(|event| {
			event["type"].as_str() == Some("m.room.member")
				&& event["state_key"].as_str() == Some(user_id.as_str())
		})
}

fn device_list_changed(response: &Value, user_id: &UserId) -> bool {
	response["device_lists"]["changed"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(Value::as_str)
		.any(|user| user == user_id.as_str())
}
