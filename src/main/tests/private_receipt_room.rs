#![cfg(test)]

mod client;

use std::net::TcpListener;

use futures::{TryFutureExt, future::join};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	ruma::{EventId, OwnedEventId, RoomId, UserId, event_id},
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

const TOKEN: &str = "private-receipt-room-regression-token";

#[test]
fn private_receipts_reject_events_from_another_room() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("allow_local_presence=false")
		.with_option("allow_outgoing_presence=false");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = foreign_room_receipts(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		run.and(outcome)
	});

	drop(runtime);
	result
}

async fn foreign_room_receipts(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;
	let user = register(services, "receiptroom", TOKEN).await?;
	let client = Client { services, base, token: TOKEN };
	let room = client.create_room(&json!({})).await?;
	let foreign = client.create_room(&json!({})).await?;
	let local_event = message(&client, &room).await?;
	let foreign_event = message(&client, &foreign).await?;
	let before = private_count(services, &room, &user).await?;

	assert!(private_count(services, &foreign, &user).await? > before);

	let unknown = event_id!("$unknown:localhost");
	let unknown = assert_receipt_status(&client, &room, unknown, 404).await?;
	let foreign = assert_receipt_status(&client, &room, &foreign_event, 404).await?;

	assert_eq!(foreign, unknown);
	assert_eq!(unknown.0["errcode"], "M_NOT_FOUND");
	assert_eq!(unknown.1["errcode"], "M_NOT_FOUND");
	assert_eq!(private_count(services, &room, &user).await?, before);
	assert_receipt_status(&client, &room, &local_event, 200).await?;
	assert_eq!(private_count(services, &room, &user).await?, before);
	Ok(())
}

async fn message(client: &Client<'_>, room: &RoomId) -> Result<OwnedEventId> {
	let response: Value = client
		.services
		.client
		.clients
		.default
		.put(client.url(&format!("rooms/{room}/send/m.room.message/receipt-room")))
		.bearer_auth(client.token)
		.json(&json!({"msgtype": "m.text", "body": "private receipt room check"}))
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(field(&response, "event_id")?.try_into()?)
}

async fn assert_receipt_status(
	client: &Client<'_>,
	room: &RoomId,
	event: &EventId,
	status: u16,
) -> Result<(Value, Value)> {
	let receipt = client
		.services
		.client
		.clients
		.default
		.post(client.url(&format!("rooms/{room}/receipt/m.read.private/{event}")))
		.bearer_auth(client.token)
		.json(&json!({}))
		.send()
		.await?;

	assert_eq!(receipt.status().as_u16(), status);

	let receipt = receipt.json().await?;
	let markers = client
		.services
		.client
		.clients
		.default
		.post(client.url(&format!("rooms/{room}/read_markers")))
		.bearer_auth(client.token)
		.json(&json!({"m.read.private": event}))
		.send()
		.await?;

	assert_eq!(markers.status().as_u16(), status);

	let markers = markers.json().await?;

	Ok((receipt, markers))
}

async fn private_count(services: &Services, room: &RoomId, user: &UserId) -> Result<u64> {
	services
		.read_receipt
		.private_read_get_count(room, user)
		.map_ok(|(count, _)| count)
		.await
}
