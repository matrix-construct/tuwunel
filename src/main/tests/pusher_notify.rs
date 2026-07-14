#![cfg(test)]

use std::{fs::remove_dir_all, process::id as process_id, str::from_utf8, time::Duration};

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
	matrix::Pdu,
	ruma::{
		DeviceId, UserId,
		api::client::push::{
			Pusher, PusherIds, PusherInit, PusherKind,
			set_pusher::v3::{PusherAction, Request as SetPusherRequest},
		},
		device_id,
		push::{HttpPusherData, PushFormat, Ruleset},
	},
	utils::stream::ReadyExt,
};
use tuwunel_service::Services;

const APP_ID: &str = "app.tuwunel.test";
const EVENT_ID: &str = "$push:remote.example";
const SENDER: &str = "@alice:remote.example";
const NOTIFY_PATH: &str = "/_matrix/push/v1/notify";

/// Inputs shared by the delivery cases; the event is driven straight through
/// the pusher service rather than through a real room and timeline.
struct Fixture<'a> {
	services: &'a Services,
	user: &'a UserId,
	device: &'a DeviceId,
	ruleset: &'a Ruleset,
	pdu: &'a Pdu,
	room_id: &'a str,
}

/// Exercises the homeserver's Push Gateway API client role end to end against a
/// stub gateway: URL validation, the full and event-id-only notification
/// formats, and the pushkey removal that honoring the gateway `rejected` list
/// requires. One server boot; the cases run sequentially.
#[test]
fn pusher_notify() -> Result {
	let db_path = format!("/tmp/tuwunel-test-pusher-notify-{}", process_id());

	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.maintenance = true;
	args.option
		.push(format!("database_path=\"{db_path}\""));
	args.option
		.push("ip_range_denylist=[]".to_owned());

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

async fn run_cases(services: &Services) -> Result {
	let server_name = services.globals.server_name();
	let user = UserId::parse_with_server_name("pushtest", server_name)?;

	services
		.users
		.create(&user, Some("password"), None)
		.await?;

	let room_id = format!("!push:{server_name}");
	let pdu = message_event(&room_id)?;
	let ruleset = Ruleset::server_default(&user);

	let fixture = Fixture {
		services,
		user: &user,
		device: device_id!("PUSHDEV"),
		ruleset: &ruleset,
		pdu: &pdu,
		room_id: &room_id,
	};

	reject_bad_url(&fixture).await?;
	full_format_delivery(&fixture).await?;
	event_id_only_delivery(&fixture).await?;
	rejected_pushkey_removed(&fixture).await?;
	foreign_rejected_key_noop(&fixture).await
}

fn message_event(room_id: &str) -> Result<Pdu> {
	serde_json::from_value(json!({
		"type": "m.room.message",
		"content": { "msgtype": "m.text", "body": "hello world" },
		"event_id": EVENT_ID,
		"room_id": room_id,
		"sender": SENDER,
		"prev_events": ["$prev:remote.example"],
		"auth_events": ["$auth:remote.example"],
		"origin_server_ts": 1_838_188_000,
		"depth": 12,
		"hashes": { "sha256": "thishashcoversallfieldsincasethisisredacted" },
	}))
	.map_err(|e| err!("invalid test pdu: {e}"))
}

/// A pusher URL that is neither http nor https is rejected at creation.
async fn reject_bad_url(fixture: &Fixture<'_>) -> Result {
	let action = pusher_action("pk-badscheme", "ftp://127.0.0.1/notify".to_owned(), false);
	let outcome = fixture
		.services
		.pusher
		.set_pusher(fixture.user, fixture.device, &action)
		.await;

	outcome
		.is_err()
		.then_some(())
		.ok_or_else(|| err!("set_pusher accepted a non-HTTP(S) pusher URL"))
}

/// A full-format notification carries the event, sender, content, and device
/// identity, and leaves the pusher in place.
async fn full_format_delivery(fixture: &Fixture<'_>) -> Result {
	let pushkey = "pk-full";
	let (path, body) = deliver(fixture, pushkey, false, r#"{"rejected":[]}"#).await?;

	if path != NOTIFY_PATH {
		return Err!("full-format notification hit unexpected path {path}");
	}

	let notification = notification(&body)?;

	expect_str(notification, "event_id", EVENT_ID)?;
	expect_str(notification, "room_id", fixture.room_id)?;
	expect_str(notification, "prio", "low")?;
	expect_str(notification, "sender", SENDER)?;

	let content = notification
		.get("content")
		.ok_or_else(|| err!("full-format notification had no content"))?;

	expect_str(content, "body", "hello world")?;

	let device = first_device(notification)?;

	expect_str(device, "app_id", APP_ID)?;
	expect_str(device, "pushkey", pushkey)?;

	fixture
		.services
		.pusher
		.get_pusher(fixture.user, pushkey)
		.await
		.map(|_| ())
		.map_err(|_| err!("full-format pusher was unexpectedly removed"))
}

/// The event-id-only format ships only the identifiers, stripping content,
/// sender, and device tweaks.
async fn event_id_only_delivery(fixture: &Fixture<'_>) -> Result {
	let pushkey = "pk-eventidonly";
	let (_path, body) = deliver(fixture, pushkey, true, r#"{"rejected":[]}"#).await?;

	let notification = notification(&body)?;

	expect_str(notification, "event_id", EVENT_ID)?;
	expect_str(notification, "room_id", fixture.room_id)?;
	expect_absent(notification, "content")?;
	expect_absent(notification, "sender")?;

	expect_absent(first_device(notification)?, "tweaks")
}

/// A pushkey the gateway names in `rejected` is removed along with its pusher.
async fn rejected_pushkey_removed(fixture: &Fixture<'_>) -> Result {
	let pushkey = "pk-rejected";
	let response = format!(r#"{{"rejected":["{pushkey}"]}}"#);
	let (path, _body) = deliver(fixture, pushkey, false, &response).await?;

	if path != NOTIFY_PATH {
		return Err!("rejected-case notification hit unexpected path {path}");
	}

	if fixture
		.services
		.pusher
		.get_pusher(fixture.user, pushkey)
		.await
		.is_ok()
	{
		return Err!("pusher survived the gateway rejecting its pushkey");
	}

	if fixture
		.services
		.pusher
		.get_pushkeys(fixture.user)
		.ready_any(|key| key == pushkey)
		.await
	{
		return Err!("get_pushkeys still yields the rejected pushkey");
	}

	Ok(())
}

/// A rejected key that is not ours leaves our pusher intact.
async fn foreign_rejected_key_noop(fixture: &Fixture<'_>) -> Result {
	let pushkey = "pk-foreign";
	deliver(fixture, pushkey, false, r#"{"rejected":["unrelated-key"]}"#).await?;

	fixture
		.services
		.pusher
		.get_pusher(fixture.user, pushkey)
		.await
		.map(|_| ())
		.map_err(|_| err!("pusher was removed for a foreign rejected key"))
}

/// Registers a pusher for `pushkey` at a fresh stub gateway, drives one
/// notification, and returns the request path and parsed body the gateway
/// received. The gateway answers `response_body`.
async fn deliver(
	fixture: &Fixture<'_>,
	pushkey: &str,
	event_id_only: bool,
	response_body: &str,
) -> Result<(String, Value)> {
	let listener = TcpListener::bind("127.0.0.1:0").await?;
	let url = format!("http://{}{NOTIFY_PATH}", listener.local_addr()?);

	let action = pusher_action(pushkey, url, event_id_only);

	fixture
		.services
		.pusher
		.set_pusher(fixture.user, fixture.device, &action)
		.await?;

	let pusher = fixture
		.services
		.pusher
		.get_pusher(fixture.user, pushkey)
		.await?;

	let (tx, mut rx) = unbounded_channel();
	let stub = spawn(stub_gateway(listener, tx, response_body.to_owned()));

	fixture
		.services
		.pusher
		.send_push_notice(fixture.user, &pusher, fixture.ruleset, fixture.pdu)
		.await?;

	let (path, body) = recv(&mut rx).await?;

	stub.abort();

	let body = serde_json::from_slice(&body)
		.map_err(|e| err!("push notification body was not json: {e}"))?;

	Ok((path, body))
}

fn pusher_action(pushkey: &str, url: String, event_id_only: bool) -> PusherAction {
	let mut data = HttpPusherData::new(url);
	data.format = event_id_only.then_some(PushFormat::EventIdOnly);

	let pusher: Pusher = PusherInit {
		ids: PusherIds::new(pushkey.to_owned(), APP_ID.to_owned()),
		kind: PusherKind::Http(data),
		app_display_name: "Tuwunel Test".into(),
		device_display_name: "Test Device".into(),
		profile_tag: None,
		lang: "en".into(),
	}
	.into();

	SetPusherRequest::post(pusher).action
}

async fn recv(rx: &mut UnboundedReceiver<(String, Vec<u8>)>) -> Result<(String, Vec<u8>)> {
	timeout(Duration::from_secs(10), rx.recv())
		.await
		.map_err(|_| err!("timed out waiting for a push notification"))?
		.ok_or_else(|| err!("stub gateway channel closed"))
}

/// Minimal push gateway: captures the request path and body from one
/// `POST /_matrix/push/v1/notify`, then answers `response_body` with a `200`.
async fn stub_gateway(
	listener: TcpListener,
	tx: UnboundedSender<(String, Vec<u8>)>,
	response_body: String,
) {
	while let Ok((mut socket, _)) = listener.accept().await {
		if let Some((path, body)) = read_request(&mut socket).await {
			tx.send((path, body)).ok();
		}

		let response = format!(
			"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
			 {}\r\nConnection: close\r\n\r\n{response_body}",
			response_body.len(),
		);

		socket.write_all(response.as_bytes()).await.ok();
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
				let path = request_path(&buf[..head_end])?;
				let body = buf[body_start..body_end].to_vec();

				return Some((path, body));
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

fn request_path(head: &[u8]) -> Option<String> {
	from_utf8(head)
		.ok()?
		.lines()
		.next()?
		.split(' ')
		.nth(1)?
		.split('?')
		.next()
		.map(ToOwned::to_owned)
}

fn notification(body: &Value) -> Result<&Value> {
	body.get("notification")
		.ok_or_else(|| err!("push body had no notification object: {body}"))
}

fn first_device(notification: &Value) -> Result<&Value> {
	notification
		.get("devices")
		.and_then(Value::as_array)
		.and_then(|devices| devices.first())
		.ok_or_else(|| err!("notification had no devices entry: {notification}"))
}

fn expect_str(value: &Value, name: &str, want: &str) -> Result {
	let got = value.get(name).and_then(Value::as_str);
	(got == Some(want))
		.then_some(())
		.ok_or_else(|| err!("field {name}: expected {want:?}, got {got:?}"))
}

fn expect_absent(value: &Value, name: &str) -> Result {
	value
		.get(name)
		.map_or(Ok(()), |found| Err!("unexpected field {name}: {found}"))
}
