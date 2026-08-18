#![cfg(test)]

use std::{
	env::var, fs::remove_dir_all, path::PathBuf, process::id as process_id, str::from_utf8,
	time::Duration,
};

use serde_json::{Value, json};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
	spawn,
	sync::mpsc::{UnboundedReceiver, UnboundedSender, error::TryRecvError, unbounded_channel},
	task::JoinHandle,
	time::{sleep, timeout},
};
use tuwunel::{Args, Runtime, Server, async_run, async_stop};
use tuwunel_core::{
	Err, Result, err,
	matrix::Pdu,
	ruma::{
		DeviceId, EventId, OwnedRoomId, RoomId, UInt, UserId,
		api::client::push::{
			Pusher, PusherIds, PusherInit, PusherKind,
			set_pusher::v3::{PusherAction, Request as SetPusherRequest},
		},
		device_id,
		presence::PresenceState,
		push::{HttpPusherData, PushFormat, Ruleset},
	},
	utils::stream::ReadyExt,
};
use tuwunel_service::{
	Services,
	presence::Ping,
	sending::Destination,
};

type CapturedRequest = (String, Vec<u8>);
type CaptureRx = UnboundedReceiver<CapturedRequest>;
type CaptureTx = UnboundedSender<CapturedRequest>;
type StubPusher = (Pusher, PusherAction, CaptureRx, AbortOnDrop);

struct StubPusherConfig<'a> {
	path: &'a str,
	event_id_only: bool,
	disable_badge_count: bool,
	response_body: &'a str,
}

const APP_ID: &str = "app.tuwunel.test";
const EVENT_ID: &str = "$push:remote.example";
const SENDER: &str = "@alice:remote.example";
const NOTIFY_PATH: &str = "/_matrix/push/v1/notify";

impl<'a> StubPusherConfig<'a> {
	fn new(response_body: &'a str) -> Self {
		Self {
			path: NOTIFY_PATH,
			event_id_only: false,
			disable_badge_count: false,
			response_body,
		}
	}
}

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
/// stub gateway: delayed-start badge recovery, URL validation, the full and
/// event-id-only notification formats, and the pushkey removal that honoring
/// the gateway `rejected` list requires.
///
/// One server boot runs the cases sequentially.
#[test]
fn pusher_notify() -> Result {
	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path =
		PathBuf::from(root).join(format!("tuwunel-test-pusher-notify-{}", process_id()));

	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.option
		.push(format!("database_path={db_path:?}"));

	args.option
		.push("ip_range_denylist=[]".to_owned());
	args.option
		.push("startup_netburst=true".to_owned());
	args.option
		.push("suppress_push_when_active=true".to_owned());

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result: Result = runtime.block_on(async {
		let services = Services::build(server.server.clone()).await?;
		let mut recovery = prepare_badge_recovery(&services).await?;
		let services = services.start().await?;
		_ = server
			.services
			.lock()
			.await
			.insert(services.clone());

		let outcome = async {
			verify_badge_recovery(&services, &mut recovery).await?;
			run_cases(&services).await
		}
		.await;

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

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
	fn drop(&mut self) { self.0.abort(); }
}

struct BadgeRecovery {
	destination: Destination,
	rx: CaptureRx,
	_stub: AbortOnDrop,
}

async fn prepare_badge_recovery(services: &Services) -> Result<BadgeRecovery> {
	let server_name = services.globals.server_name();
	let user = UserId::parse_with_server_name("badge-recovery", server_name)?;
	let pushkey = "pk-badge-recovery";

	let listener = TcpListener::bind("127.0.0.1:0").await?;
	let url = format!("http://{}{NOTIFY_PATH}", listener.local_addr()?);
	let action = pusher_action(pushkey, url, false, false);

	services
		.pusher
		.set_pusher(&user, device_id!("BADGERECOVERY"), &action)
		.await?;

	let (tx, rx) = unbounded_channel();
	let stub = AbortOnDrop(spawn(stub_gateway(listener, tx, r#"{"rejected":[]}"#.to_owned())));

	services.sending.refresh_push_badge(&user).await?;
	services.sending.refresh_push_badge(&user).await?;

	Ok(BadgeRecovery {
		destination: Destination::Push(user, pushkey.to_owned()),
		rx,
		_stub: stub,
	})
}

async fn verify_badge_recovery(services: &Services, recovery: &mut BadgeRecovery) -> Result {
	let (path, body) = recv(&mut recovery.rx).await?;

	if path != NOTIFY_PATH {
		return Err!("recovered badge notification hit unexpected path {path}");
	}

	let body = serde_json::from_slice(&body)
		.map_err(|e| err!("recovered badge notification body was not json: {e}"))?;

	let recovered = notification(&body)?;

	assert_eq!(recovered.get("counts"), Some(&json!({"unread": 0})));

	for field in ["event_id", "room_id", "sender", "type", "content", "prio"] {
		expect_absent(recovered, field)?;
	}

	expect_absent(first_device(recovered)?, "tweaks")?;
	wait_for_badge_queue_cleanup(services, &recovery.destination).await?;

	let Destination::Push(user_id, _) = &recovery.destination else {
		unreachable!("badge recovery destination is push");
	};

	// A changed count keeps the barrier an observed POST under the delivery
	// memo, and proves the memo-differs arm on the same wire.
	let server_name = services.globals.server_name();
	let room_id = OwnedRoomId::from_parts('!', "badge-recovery", Some(server_name.as_str()))?;
	let joined = services.db.get("userroomid_joined")?;
	let unread = services.db.get("userroomid_notificationcount")?;

	joined.put((user_id, &room_id), 1_u64);
	unread.put((user_id, &room_id), 1_u64);

	// This wake sits behind the two pre-start messages in the worker channel.
	// Its completed row is a deterministic barrier for stale-wake processing.
	services
		.sending
		.refresh_push_badge(user_id)
		.await?;

	wait_for_badge_queue_cleanup(services, &recovery.destination).await?;

	let (path, body) = recv(&mut recovery.rx).await?;

	if path != NOTIFY_PATH {
		return Err!("barrier badge notification hit unexpected path {path}");
	}

	let body = serde_json::from_slice(&body)
		.map_err(|e| err!("barrier badge notification body was not json: {e}"))?;

	assert_eq!(notification(&body)?.get("counts"), Some(&json!({"unread": 1})));

	match recovery.rx.try_recv() {
		| Err(TryRecvError::Empty) => Ok(()),
		| Err(TryRecvError::Disconnected) =>
			Err!("stub gateway channel closed after badge recovery"),
		| Ok(_) => Err!("stale badge wake produced a duplicate notification"),
	}
}

async fn wait_for_badge_queue_cleanup(services: &Services, destination: &Destination) -> Result {
	let cleared = async {
		loop {
			let queued = services
				.sending
				.db
				.queued_requests(destination)
				.ready_any(|_| true)
				.await;

			let active = services
				.sending
				.db
				.active_requests_for(destination)
				.ready_any(|_| true)
				.await;

			if !queued && !active {
				break;
			}

			sleep(Duration::from_millis(10)).await;
		}
	};

	timeout(Duration::from_secs(10), cleared)
		.await
		.map_err(|_| err!("timed out waiting for recovered badge queue cleanup"))
}

async fn run_cases(services: &Services) -> Result {
	let server_name = services.globals.server_name();
	let user = UserId::parse_with_server_name("pushtest", server_name)?;

	services
		.users
		.create(&user, Some("password"), None)
		.await?;

	let room_id = RoomId::parse(format!("!push:{server_name}"))?;
	let other_room_id = RoomId::parse(format!("!push-other:{server_name}"))?;
	let joined = services.db.get("userroomid_joined")?;
	let unread = services.db.get("userroomid_notificationcount")?;

	for room_id in [&room_id, &other_room_id] {
		joined.put((&user, room_id), 1_u64);
		unread.put((&user, room_id), 1_u64);
	}

	let pdu = message_event(room_id.as_str())?;
	let ruleset = Ruleset::server_default(&user);

	let fixture = Fixture {
		services,
		user: &user,
		device: device_id!("PUSHDEV"),
		ruleset: &ruleset,
		pdu: &pdu,
		room_id: room_id.as_str(),
	};

	reject_bad_url(&fixture).await?;
	full_format_delivery(&fixture).await?;
	event_id_only_delivery(&fixture).await?;
	gateway_url_paths(&fixture).await?;
	rejected_pushkey_removed(&fixture).await?;
	foreign_rejected_key_noop(&fixture).await?;
	legacy_actions(&fixture).await?;
	counts_only_delivery(&fixture, &room_id, &other_room_id).await?;
	account_wide_count_delivery(&fixture, &room_id).await?;
	badge_count_opt_out(&fixture).await?;
	badge_delivery_memo(&fixture, &room_id).await?;
	badge_bypasses_suppression(&fixture).await
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
	let action = pusher_action("pk-badscheme", "ftp://127.0.0.1/notify".to_owned(), false, false);

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
	let (path, body) = deliver(fixture, pushkey, false, false, r#"{"rejected":[]}"#).await?;

	if path != NOTIFY_PATH {
		return Err!("full-format notification hit unexpected path {path}");
	}

	let notification = notification(&body)?;

	expect_str(notification, "event_id", EVENT_ID)?;
	expect_str(notification, "room_id", fixture.room_id)?;
	expect_str(notification, "prio", "low")?;
	expect_str(notification, "sender", SENDER)?;

	assert_eq!(notification.get("counts"), Some(&json!({"unread": 2})));

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
	let (_path, body) = deliver(fixture, pushkey, true, false, r#"{"rejected":[]}"#).await?;

	let notification = notification(&body)?;

	expect_str(notification, "event_id", EVENT_ID)?;
	expect_str(notification, "room_id", fixture.room_id)?;
	expect_absent(notification, "content")?;
	expect_absent(notification, "sender")?;

	expect_absent(first_device(notification)?, "tweaks")
}

async fn gateway_url_paths(fixture: &Fixture<'_>) -> Result {
	let cases = [
		(
			"pk-url-mid-path",
			"/_matrix/push/v1/notify/gw",
			"/_matrix/push/v1/notify/gw/_matrix/push/v1/notify",
		),
		("pk-url-prefix", "/gw/_matrix/push/v1/notify", "/gw/_matrix/push/v1/notify"),
		("pk-url-trailing-slash", "/_matrix/push/v1/notify/", NOTIFY_PATH),
	];

	for (pushkey, registered_path, expected_path) in cases {
		let config = StubPusherConfig {
			path: registered_path,
			..StubPusherConfig::new(r#"{"rejected":[]}"#)
		};
		let (pusher, _action, mut rx, _stub) = stub_pusher(fixture, pushkey, config).await?;

		fixture
			.services
			.pusher
			.send_push_notice(fixture.user, &pusher, fixture.ruleset, fixture.pdu)
			.await?;

		let (request_path, _body) = recv(&mut rx).await?;

		if request_path != expected_path {
			return Err!(
				"path {registered_path} produced {request_path}, expected {expected_path}"
			);
		}
	}

	Ok(())
}

/// A pushkey the gateway names in `rejected` is removed along with its pusher.
async fn rejected_pushkey_removed(fixture: &Fixture<'_>) -> Result {
	let pushkey = "pk-rejected";
	let response = format!(r#"{{"rejected":["{pushkey}"]}}"#);
	let (path, _body) = deliver(fixture, pushkey, false, false, &response).await?;

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
	deliver(fixture, pushkey, false, false, r#"{"rejected":["unrelated-key"]}"#).await?;

	fixture
		.services
		.pusher
		.get_pusher(fixture.user, pushkey)
		.await
		.map(|_| ())
		.map_err(|_| err!("pusher was removed for a foreign rejected key"))
}

async fn legacy_actions(fixture: &Fixture<'_>) -> Result {
	let pushkey = "pk-legacy-actions";
	let config = StubPusherConfig::new(r#"{"rejected":[]}"#);
	let (pusher, _action, mut rx, _stub) = stub_pusher(fixture, pushkey, config).await?;

	let notify_ruleset = legacy_ruleset(&json!(["notify", "dont_notify"]))?;

	fixture
		.services
		.pusher
		.send_push_notice(fixture.user, &pusher, &notify_ruleset, fixture.pdu)
		.await?;

	let (_path, body) = recv(&mut rx).await?;
	let body = serde_json::from_slice(&body)
		.map_err(|e| err!("legacy-action notification body was not json: {e}"))?;

	expect_str(notification(&body)?, "event_id", EVENT_ID)?;

	let quiet_ruleset = legacy_ruleset(&json!(["dont_notify", "coalesce"]))?;

	fixture
		.services
		.pusher
		.send_push_notice(fixture.user, &pusher, &quiet_ruleset, fixture.pdu)
		.await?;

	expect_quiescent(&mut rx, "quiet legacy actions")
}

fn legacy_ruleset(actions: &Value) -> Result<Ruleset> {
	serde_json::from_value(json!({
		"override": [{
			"rule_id": "tuwunel.test.legacy",
			"default": false,
			"enabled": true,
			"conditions": [],
			"actions": actions,
		}],
	}))
	.map_err(|e| err!("invalid test ruleset: {e}"))
}

async fn counts_only_delivery(
	fixture: &Fixture<'_>,
	room_id: &RoomId,
	other_room_id: &RoomId,
) -> Result {
	let pusher = &fixture.services.pusher;

	pusher
		.reset_notification_counts(fixture.user, room_id)
		.await;

	pusher
		.reset_notification_counts(fixture.user, other_room_id)
		.await;

	let remaining = pusher
		.global_notification_count(fixture.user)
		.await;

	if remaining != 0 {
		return Err!("reset left an account-wide unread total of {remaining}");
	}

	let (path, body) =
		deliver(fixture, "pk-badge-zero", false, true, r#"{"rejected":[]}"#).await?;

	if path != NOTIFY_PATH {
		return Err!("counts-only notification hit unexpected path {path}");
	}

	let notification = notification(&body)?;

	assert_eq!(notification.get("counts"), Some(&json!({"unread": 0})));

	for field in ["event_id", "room_id", "sender", "type", "content", "prio"] {
		expect_absent(notification, field)?;
	}

	expect_absent(first_device(notification)?, "tweaks")
}

async fn account_wide_count_delivery(fixture: &Fixture<'_>, room_id: &RoomId) -> Result {
	let unread = fixture
		.services
		.db
		.get("userroomid_notificationcount")?;

	let root = EventId::parse(EVENT_ID)?;
	let stale_room_id =
		RoomId::parse(format!("!push-stale:{}", fixture.services.globals.server_name()))?;

	unread.put((fixture.user, room_id, &root), 7_u64);
	unread.put((fixture.user, &stale_room_id), 41_u64);

	let (_, body) =
		deliver(fixture, "pk-badge-thread", false, true, r#"{"rejected":[]}"#).await?;

	let thread_notification = notification(&body)?;

	assert_eq!(thread_notification.get("counts"), Some(&json!({"unread": 7})));

	unread.put((fixture.user, room_id), u64::MAX);

	let (_, body) = deliver(fixture, "pk-badge-max", false, true, r#"{"rejected":[]}"#).await?;
	let max_notification = notification(&body)?;

	assert_eq!(max_notification.get("counts"), Some(&json!({"unread": UInt::MAX})));

	Ok(())
}

async fn badge_count_opt_out(fixture: &Fixture<'_>) -> Result {
	let pushkey = "pk-badge-disabled";
	let config = StubPusherConfig {
		disable_badge_count: true,
		..StubPusherConfig::new(r#"{"rejected":[]}"#)
	};
	let (pusher, _action, mut rx, _stub) = stub_pusher(fixture, pushkey, config).await?;

	fixture
		.services
		.pusher
		.send_push_notice(fixture.user, &pusher, fixture.ruleset, fixture.pdu)
		.await?;

	let (path, body) = recv(&mut rx).await?;
	if path != NOTIFY_PATH {
		return Err!("badge opt-out notification hit unexpected path {path}");
	}

	let body = serde_json::from_slice(&body)
		.map_err(|e| err!("badge opt-out notification body was not json: {e}"))?;

	expect_absent(notification(&body)?, "counts")?;

	fixture
		.services
		.pusher
		.send_badge_notice(fixture.user, &pusher)
		.await?;

	match rx.try_recv() {
		| Err(TryRecvError::Empty) => Ok(()),
		| Err(TryRecvError::Disconnected) =>
			Err!("stub gateway channel closed after badge opt-out"),
		| Ok(_) => Err!("badge opt-out emitted a counts-only notification"),
	}
}

/// Covers the badge delivery memo.
///
/// An unchanged total is not re-sent, a changed total is, an event
/// notification stamps the memo, and pusher replacement forgets it.
async fn badge_delivery_memo(fixture: &Fixture<'_>, room_id: &RoomId) -> Result {
	let unread = fixture
		.services
		.db
		.get("userroomid_notificationcount")?;

	let root = EventId::parse(EVENT_ID)?;

	unread.put((fixture.user, room_id), 0_u64);
	unread.put((fixture.user, room_id, &root), 0_u64);

	let pushkey = "pk-badge-memo";
	let config = StubPusherConfig::new(r#"{"rejected":[]}"#);
	let (pusher, action, mut rx, _stub) = stub_pusher(fixture, pushkey, config).await?;

	let refresh = || {
		fixture
			.services
			.pusher
			.send_badge_notice(fixture.user, &pusher)
	};

	refresh().await?;

	let (_, body) = recv(&mut rx).await?;
	let body = serde_json::from_slice(&body)
		.map_err(|e| err!("first memo delivery body was not json: {e}"))?;

	assert_eq!(notification(&body)?.get("counts"), Some(&json!({"unread": 0})));

	refresh().await?;
	match rx.try_recv() {
		| Err(TryRecvError::Empty) => (),
		| Err(TryRecvError::Disconnected) =>
			return Err!("stub gateway channel closed during memo dedupe"),
		| Ok(_) => return Err!("unchanged badge total was re-sent to the gateway"),
	}

	unread.put((fixture.user, room_id), 3_u64);
	refresh().await?;

	let (_, body) = recv(&mut rx).await?;
	let body = serde_json::from_slice(&body)
		.map_err(|e| err!("changed memo delivery body was not json: {e}"))?;

	assert_eq!(notification(&body)?.get("counts"), Some(&json!({"unread": 3})));

	// The event notification carries the new total and stamps the memo, so
	// the refresh behind it has nothing to add.
	unread.put((fixture.user, room_id), 5_u64);
	fixture
		.services
		.pusher
		.send_push_notice(fixture.user, &pusher, fixture.ruleset, fixture.pdu)
		.await?;

	recv(&mut rx).await?;

	refresh().await?;
	match rx.try_recv() {
		| Err(TryRecvError::Empty) => (),
		| Err(TryRecvError::Disconnected) =>
			return Err!("stub gateway channel closed after the event notice"),
		| Ok(_) => return Err!("event-stamped badge total was re-sent to the gateway"),
	}

	// Replacement forgets the record: the unchanged total is sent again.
	fixture
		.services
		.pusher
		.set_pusher(fixture.user, fixture.device, &action)
		.await?;

	refresh().await?;

	let (path, _) = recv(&mut rx).await?;

	if path != NOTIFY_PATH {
		return Err!("post-replacement badge notification hit unexpected path {path}");
	}

	Ok(())
}

/// Counts-only refreshes deliver while pushes are suppressed.
///
/// An active user's reads must still reconcile the gateway; only event
/// notifications are deferred by suppression.
async fn badge_bypasses_suppression(fixture: &Fixture<'_>) -> Result {
	let pushkey = "pk-badge-suppressed";
	let config = StubPusherConfig::new(r#"{"rejected":[]}"#);
	let (_pusher, _action, mut rx, _stub) = stub_pusher(fixture, pushkey, config).await?;

	// Drive the active heuristic: online presence plus a fresh sync stamp.
	fixture
		.services
		.presence
		.maybe_ping_presence(fixture.user, Ping::default())
		.await?;

	fixture
		.services
		.presence
		.note_sync(fixture.user, None)
		.await;

	// Assert the heuristic's inputs registered, or a driving failure would
	// let the POST arrive unsuppressed and pass this case vacuously.
	let presence = fixture
		.services
		.presence
		.get_presence(fixture.user)
		.await?;

	if presence.content.presence != PresenceState::Online {
		return Err!("fixture user's presence did not register as online");
	}

	if presence
		.content
		.last_active_ago
		.is_none_or(|age| u64::from(age) >= 65_000)
	{
		return Err!("fixture user's presence is not inside the active window");
	}

	if fixture
		.services
		.presence
		.last_sync_gap_ms(fixture.user)
		.await
		.is_none_or(|gap| gap >= 32_000)
	{
		return Err!("fixture user's sync activity is not inside the active window");
	}

	fixture
		.services
		.sending
		.refresh_push_badge(fixture.user)
		.await?;

	let (path, _) = recv(&mut rx).await?;

	if path != NOTIFY_PATH {
		return Err!("suppressed-window badge notification hit unexpected path {path}");
	}

	Ok(())
}

/// Registers a pusher for `pushkey` at a fresh stub gateway, drives one
/// notification, and returns the request path and parsed body the gateway
/// received. The gateway answers `response_body`.
async fn deliver(
	fixture: &Fixture<'_>,
	pushkey: &str,
	event_id_only: bool,
	badge_only: bool,
	response_body: &str,
) -> Result<(String, Value)> {
	let config = StubPusherConfig {
		event_id_only,
		..StubPusherConfig::new(response_body)
	};
	let (pusher, _action, mut rx, _stub) = stub_pusher(fixture, pushkey, config).await?;

	let pusher_service = &fixture.services.pusher;

	match badge_only {
		| true =>
			pusher_service
				.send_badge_notice(fixture.user, &pusher)
				.await?,
		| false =>
			pusher_service
				.send_push_notice(fixture.user, &pusher, fixture.ruleset, fixture.pdu)
				.await?,
	}

	let (path, body) = recv(&mut rx).await?;

	let body = serde_json::from_slice(&body)
		.map_err(|e| err!("push notification body was not json: {e}"))?;

	Ok((path, body))
}

/// Registers a pusher for `pushkey` at a fresh stub gateway.
///
/// Returns the stored pusher, the action for re-registration, and the
/// gateway's capture channel; the stub aborts when its handle drops.
async fn stub_pusher(
	fixture: &Fixture<'_>,
	pushkey: &str,
	config: StubPusherConfig<'_>,
) -> Result<StubPusher> {
	let listener = TcpListener::bind("127.0.0.1:0").await?;
	let url = format!("http://{}{}", listener.local_addr()?, config.path);
	let action = pusher_action(pushkey, url, config.event_id_only, config.disable_badge_count);

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

	let (tx, rx) = unbounded_channel();
	let stub = AbortOnDrop(spawn(stub_gateway(listener, tx, config.response_body.to_owned())));

	Ok((pusher, action, rx, stub))
}

fn pusher_action(
	pushkey: &str,
	url: String,
	event_id_only: bool,
	disable_badge_count: bool,
) -> PusherAction {
	let mut data = HttpPusherData::new(url);
	data.format = event_id_only.then_some(PushFormat::EventIdOnly);
	data.data
		.insert("disable_badge_count".into(), Value::Bool(disable_badge_count));

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

async fn recv(rx: &mut CaptureRx) -> Result<CapturedRequest> {
	timeout(Duration::from_secs(10), rx.recv())
		.await
		.map_err(|_| err!("timed out waiting for a push notification"))?
		.ok_or_else(|| err!("stub gateway channel closed"))
}

fn expect_quiescent(rx: &mut CaptureRx, case: &str) -> Result {
	match rx.try_recv() {
		| Err(TryRecvError::Empty) => Ok(()),
		| Err(TryRecvError::Disconnected) => Err!("stub gateway channel closed after {case}"),
		| Ok(_) => Err!("{case} produced a spurious notification"),
	}
}

/// Minimal push gateway: captures the request path and body from one
/// `POST /_matrix/push/v1/notify`, then answers `response_body` with a `200`.
async fn stub_gateway(
	listener: TcpListener,
	tx: CaptureTx,
	response_body: String,
) {
	let response = format!(
		"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: \
		 close\r\n\r\n{response_body}",
		response_body.len(),
	);

	while let Ok((mut socket, _)) = listener.accept().await {
		if let Some((path, body)) = read_request(&mut socket).await {
			tx.send((path, body)).ok();
		}

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
