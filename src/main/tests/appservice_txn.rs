#![cfg(test)]

use std::{
	fs::remove_dir_all, iter::once, process::id as process_id, str::from_utf8, sync::Arc,
	time::Duration,
};

use serde_json::{Value, json};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
	spawn,
	sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
	time::timeout,
};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err,
	ruma::{
		DeviceId, UserId,
		api::appservice::{Namespace, Namespaces, Registration, RegistrationInit},
		device_id,
	},
};
use tuwunel_service::Services;

const TO_DEVICE: &str = "de.sorunome.msc2409.to_device";

/// Boots a server with a stub appservice listener and asserts the transactions
/// it receives: to-device delivery (MSC4203) routed by user namespace, and
/// stable-yet-distinct transaction IDs.
#[test]
fn appservice_e2ee_transactions() -> Result {
	let db_path = format!("/tmp/tuwunel-test-appservice-txn-{}", process_id());

	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.maintenance = true;
	args.option
		.push(format!("database_path=\"{db_path}\""));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = async_start(&server).await?;

		let outcome = run_cases(&services).await;

		server.server.shutdown()?;
		drop(services);

		async_run(&server).await?;
		async_stop(&server).await?;

		outcome
	});

	drop(runtime);
	remove_dir_all(&db_path).ok();

	result
}

async fn run_cases(services: &Arc<Services>) -> Result {
	let listener = TcpListener::bind("127.0.0.1:0").await?;
	let url = format!("http://{}", listener.local_addr()?);

	let (tx, mut rx) = unbounded_channel();
	let stub = spawn(stub_appservice(listener, tx));

	let server_name = services.globals.server_name();
	register(services, "voicebridge_on", &url, "_von", "@_von_.*").await?;
	register(services, "voicebridge_off", &url, "_voff", "@_voff_.*").await?;

	let on_ghost = UserId::parse_with_server_name("_von_ghost", server_name)?;
	let off_ghost = UserId::parse_with_server_name("_voff_ghost", server_name)?;
	let sender = UserId::parse("@alice:remote.example")?;
	let device = device_id!("GHOSTDEV");
	let content = json!({ "algorithm": "m.olm.v1.curve25519-aes-sha2", "ciphertext": {} });

	// Each bridge receives exactly the to-device events addressed into its own
	// user namespace.
	send_to_device(services, &sender, &on_ghost, device, &content).await?;
	let (first_txn_id, body) = recv_txn(&mut rx).await?;
	let txn = parse(&body)?;
	assert_to_device(&txn, &on_ghost, device, &sender)?;

	send_to_device(services, &sender, &off_ghost, device, &content).await?;
	let (_, body) = recv_txn(&mut rx).await?;
	let txn = parse(&body)?;
	assert_to_device(&txn, &off_ghost, device, &sender)?;

	// A byte-identical repeat still ships under a distinct transaction ID so the
	// bridge's txn-id dedup does not drop it.
	send_to_device(services, &sender, &on_ghost, device, &content).await?;
	let (second_txn_id, _) = recv_txn(&mut rx).await?;
	if first_txn_id == second_txn_id {
		return Err!("identical consecutive transactions shared txn id {first_txn_id}");
	}

	stub.abort();

	Ok(())
}

async fn send_to_device(
	services: &Services,
	sender: &UserId,
	target: &UserId,
	device: &DeviceId,
	content: &Value,
) -> Result {
	let count =
		services
			.users
			.add_to_device_event(sender, target, device, "m.room.encrypted", content);

	services
		.sending
		.send_to_device_appservices(
			sender,
			target,
			once((device, count)),
			"m.room.encrypted",
			content,
		)
		.await
}

async fn register(
	services: &Services,
	id: &str,
	url: &str,
	sender_localpart: &str,
	user_regex: &str,
) -> Result {
	let mut namespaces = Namespaces::new();
	namespaces.users = vec![Namespace::new(true, user_regex.to_owned())];

	let registration: Registration = RegistrationInit {
		id: id.to_owned(),
		url: Some(url.to_owned()),
		as_token: format!("{id}_as_token"),
		hs_token: format!("{id}_hs_token"),
		sender_localpart: sender_localpart.to_owned(),
		namespaces,
		rate_limited: None,
		protocols: None,
	}
	.into();

	services
		.appservice
		.register_appservice(registration)
		.await
}

async fn recv_txn(rx: &mut UnboundedReceiver<(String, Vec<u8>)>) -> Result<(String, Vec<u8>)> {
	timeout(Duration::from_secs(10), rx.recv())
		.await
		.map_err(|_| err!("timed out waiting for an appservice transaction"))?
		.ok_or_else(|| err!("stub appservice channel closed"))
}

fn parse(body: &[u8]) -> Result<Value> {
	serde_json::from_slice(body)
		.map_err(|e| err!("appservice transaction body was not json: {e}"))
}

fn assert_to_device(txn: &Value, target: &UserId, device: &DeviceId, sender: &UserId) -> Result {
	let event = txn
		.get(TO_DEVICE)
		.and_then(Value::as_array)
		.and_then(|events| events.first())
		.ok_or_else(|| err!("transaction had no {TO_DEVICE} entry"))?;

	let field = |name: &str| {
		event
			.get(name)
			.and_then(Value::as_str)
			.map(ToOwned::to_owned)
			.unwrap_or_default()
	};

	if field("type") != "m.room.encrypted"
		|| field("sender") != sender.as_str()
		|| field("to_user_id") != target.as_str()
		|| field("to_device_id") != device.as_str()
		|| event.get("content").is_none()
	{
		return Err!("unexpected to-device entry: {event}");
	}

	Ok(())
}

/// Minimal appservice transaction endpoint: captures the `{txnId}` path segment
/// and the request body from each `PUT /transactions/{txnId}`, then answers an
/// empty `200`.
async fn stub_appservice(listener: TcpListener, tx: UnboundedSender<(String, Vec<u8>)>) {
	while let Ok((mut socket, _)) = listener.accept().await {
		if let Some((txn_id, body)) = read_request(&mut socket).await {
			tx.send((txn_id, body)).ok();
		}

		socket
			.write_all(
				b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
			)
			.await
			.ok();
		socket.flush().await.ok();
	}
}

async fn read_request(socket: &mut TcpStream) -> Option<(String, Vec<u8>)> {
	let mut buf = Vec::new();
	let mut chunk = [0_u8; 4096];
	loop {
		if let Some(head_end) = find(&buf, b"\r\n\r\n") {
			let content_length = content_length(&buf[..head_end])?;
			let body_start = head_end.checked_add(4)?;
			let body_end = body_start.checked_add(content_length)?;
			if buf.len() >= body_end {
				let txn_id = txn_id(&buf[..head_end])?;
				let body = buf[body_start..body_end].to_vec();

				return Some((txn_id, body));
			}
		}

		let read = socket.read(&mut chunk).await.ok()?;
		if read == 0 {
			return None;
		}

		buf.extend_from_slice(&chunk[..read]);
	}
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack
		.windows(needle.len())
		.position(|window| window == needle)
}

fn content_length(head: &[u8]) -> Option<usize> {
	from_utf8(head)
		.ok()?
		.lines()
		.find_map(|line| {
			line.split_once(':')
				.filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
		})
		.and_then(|(_, value)| value.trim().parse().ok())
}

fn txn_id(head: &[u8]) -> Option<String> {
	from_utf8(head)
		.ok()?
		.lines()
		.next()?
		.split(' ')
		.nth(1)?
		.rsplit('/')
		.next()?
		.split('?')
		.next()
		.map(ToOwned::to_owned)
}
