#![cfg(test)]

//! Delayed events (MSC4140) end to end, on a booted server.
//!
//! The cases cover discovery, scheduling through every request form, firing
//! no earlier than the delay, management by path and by body, a tokenless
//! send, owner checks, the delay ID shown to the sender alone, limits,
//! deactivation, and a scheduled event surviving a restart.

use std::{
	env::{current_exe, var},
	fs::remove_dir_all,
	path::{Path, PathBuf},
	process::Command,
	time::Duration,
};

use futures::{StreamExt, future::join3};
use reqwest::{Method, Response, StatusCode, header::RETRY_AFTER};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel::Args;
use tuwunel_core::{
	Err, Result, err, implement, is_equal_to,
	ruma::{
		RoomAliasId, RoomId, UserId,
		events::{
			StateEventType,
			room::{member::RoomMemberEventContent, topic::RoomTopicEventContent},
		},
	},
	utils::time::now_millis,
};
use tuwunel_matrix::Event;
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, poll_until, register},
	fixture::{boot, boot_at},
	require::require,
};

mod client;
mod fixture;

#[path = "client/require.rs"]
mod require;

const ALICE: &str = "delayed-events-alice-token-000000";
const BOB: &str = "delayed-events-bob-token-00000000";
const QUIET: [&str; 2] = ["logging.enable=false", "database.pool.max_workers=64"];
const UNSTABLE: &str = "unstable/org.matrix.msc4140/";
const CALL_MEMBER: &str = "org.matrix.msc3401.call.member";
const DELAY_ID: &str = "org.matrix.msc4140.delay_id";
const FLAGS: [&str; 2] = ["org.matrix.msc4140", "org.matrix.msc4140.stable"];
const CAPABILITIES: [&str; 2] = ["m.delayed_events", "org.matrix.msc4140.delayed_events"];
const CHILD_DATABASE_ENV: &str = "DELAYED_EVENTS_TEST_DATABASE";
const CHILD_PHASE_ENV: &str = "DELAYED_EVENTS_TEST_PHASE";
const FIRE_DEADLINE: Duration = Duration::from_secs(10);

// Long enough that nothing fires while a case inspects it.
const LONG: u64 = 60_000;

/// Runs every case, each on a server of its own.
///
/// The cases boot in turn, and each restart phase boots in a child process,
/// since a database closed by this process stays locked to it.
#[test]
fn delayed_events() -> Result {
	if let Ok(phase) = var(CHILD_PHASE_ENV) {
		return restart_phase(&phase);
	}

	let limited = QUIET
		.into_iter()
		.chain(["rooms.delayed_events.max_delay_s=120", "rooms.delayed_events.max_scheduled=1"]);

	let switched_off = QUIET
		.into_iter()
		.chain(["rooms.delayed_events.max_scheduled=0"]);

	boot("delayed-events", QUIET, exercise)?;
	boot("delayed-events-limits", limited, limits)?;
	boot("delayed-events-disabled", switched_off, disabled)?;

	survive_a_restart()
}

fn restart_phase(phase: &str) -> Result {
	let database = var(CHILD_DATABASE_ENV).expect("delayed events child database is set");
	let database = PathBuf::from(database);

	match phase {
		| "schedule" => boot_at(&database, &["fresh"], QUIET, schedule_before_restart),
		| "fire" => {
			let started = now_millis();

			boot_at(&database, &[], QUIET, async |services: &Services, _: &str| {
				fired_after_restart(services, started).await
			})
		},
		| _ => Err!("unknown delayed events phase: {phase}"),
	}
}

async fn exercise(services: &Services, base: &str) -> Result {
	let alice_id = register(services, "alice", ALICE).await?;
	let bob_id = register(services, "bob", BOB).await?;

	let alice = Client { services, base, token: ALICE };
	let bob = Client { services, base, token: BOB };
	let room = alice.create_room(&json!({})).await?;

	discovery_advertised(&alice).await?;
	empty_key_query_fires(services, &alice, &room).await?;
	every_form_schedules(&alice, &room).await?;
	bare_delay_refused(&alice, &room).await?;
	restart_and_cancel(&alice, &room).await?;
	tokenless_send_on_unstable_only(services, &alice, &room, &alice_id).await?;
	owner_mismatch_is_not_found(&alice, &bob, &room).await?;
	transaction_retry_returns_one_delay(services, &alice, &room, &alice_id).await?;
	room_auth_checked_at_schedule(&bob, &room).await?;
	delay_id_reaches_only_its_sender(services, &alice, &bob, &alice_id).await?;
	deactivation_removes_scheduled(services, &bob, &bob_id).await
}

async fn discovery_advertised(alice: &Client<'_>) -> Result {
	let versions = alice
		.get_json(&alice.client_url("", "versions"))
		.await?;

	for flag in FLAGS {
		assert_eq!(versions["unstable_features"][flag], json!(true), "{flag}: {versions}");
	}

	let capabilities = alice.get_json(&alice.url("capabilities")).await?;

	for name in CAPABILITIES {
		let capability = &capabilities["capabilities"][name];

		assert_eq!(capability["max_delay_ms"], json!(86_400_000), "{name}: {capabilities}");
		assert_eq!(capability["max_scheduled"], json!(100), "{name}: {capabilities}");
	}

	Ok(())
}

async fn empty_key_query_fires(services: &Services, alice: &Client<'_>, room: &RoomId) -> Result {
	let delay = 300;
	let path = format!("rooms/{room}/state/m.room.topic?org.matrix.msc4140.delay={delay}");
	let scheduled_at = now_millis();
	let reply = alice
		.put_json(&alice.url(&path), &json!({ "topic": "later" }))
		.await?;

	field(&reply, "delay_id")?;

	let fired = poll_until(FIRE_DEADLINE, async || {
		topic(services, room).await.as_deref() == Some("later")
	})
	.await;

	require(fired, "the delayed topic was never sent")?;
	require(
		topic_sent_at(services, room)
			.await?
			.ge(&scheduled_at.saturating_add(delay)),
		"the delayed topic was sent before its delay",
	)
}

async fn every_form_schedules(alice: &Client<'_>, room: &RoomId) -> Result {
	let delay = format!("org.matrix.msc4140.delay={LONG}");
	let message = alice.url(&format!("rooms/{room}/send/m.room.message/query?{delay}"));
	let state = alice.url(&format!("rooms/{room}/state/m.room.topic/?{delay}"));
	let stable = alice.url(&format!("rooms/{room}/delayed_event/m.room.message/stable"));
	let dedicated = json!({ "delay_ms": LONG, "content": { "body": "x" } });

	let message = alice
		.put_json(&message, &json!({ "body": "x" }))
		.await?;

	let state = alice
		.put_json(&state, &json!({ "topic": "x" }))
		.await?;

	let stable = alice.put_json(&stable, &dedicated).await?;
	let forms = [
		(message, "m.room.message", None),
		(state, "m.room.topic", Some("")),
		(stable, "m.room.message", None),
	];

	for (reply, event_type, state_key) in forms {
		let delay_id = field(&reply, "delay_id")?;
		let event = alice.delayed(delay_id).await?;

		assert_eq!(event["type"], event_type, "{event}");
		assert_eq!(event.get("state_key").and_then(Value::as_str), state_key, "{event}");

		let event_url = alice.client_url(UNSTABLE, &format!("delayed_events/{delay_id}"));

		alice
			.post_json(&event_url, &json!({ "action": "cancel" }))
			.await?;

		refused(alice.fetch(&event_url).await?, StatusCode::NOT_FOUND, "M_NOT_FOUND").await?;
	}

	Ok(())
}

async fn bare_delay_refused(alice: &Client<'_>, room: &RoomId) -> Result {
	let path = format!("rooms/{room}/state/m.room.topic/?org.matrix.msc4140.delay");
	let response = alice
		.request(Method::PUT, &alice.url(&path), &json!({ "topic": "now" }))
		.await?;

	refused(response, StatusCode::BAD_REQUEST, "M_INVALID_PARAM").await
}

async fn restart_and_cancel(alice: &Client<'_>, room: &RoomId) -> Result {
	let delay_id = alice
		.schedule_dedicated(room, "m.room.message", "restart", &json!({ "body": "x" }))
		.await?;

	let event = alice.delayed(&delay_id).await?;

	assert_eq!(event["delay_ms"], json!(LONG), "{event}");
	assert_eq!(event["type"], "m.room.message", "{event}");

	let scheduled_at = event["delayed_since_ts"].as_u64();

	sleep(Duration::from_millis(20)).await;
	alice.act(&delay_id, "restart").await?;

	let restarted = alice.delayed(&delay_id).await?;

	require(
		restarted["delayed_since_ts"].as_u64() > scheduled_at,
		"a restart left delayed_since_ts in place",
	)?;

	let listed = alice
		.get_json(&alice.client_url(UNSTABLE, "delayed_events"))
		.await?;

	assert_eq!(listed["delayed_events"][0]["delay_id"], delay_id.as_str(), "{listed}");
	assert_eq!(listed["delayed_events"][0]["delay"], json!(LONG), "{listed}");

	alice.act(&delay_id, "cancel").await?;

	let response = alice
		.fetch(&alice.client_url(UNSTABLE, &format!("delayed_events/{delay_id}")))
		.await?;

	refused(response, StatusCode::NOT_FOUND, "M_NOT_FOUND").await
}

async fn tokenless_send_on_unstable_only(
	services: &Services,
	alice: &Client<'_>,
	room: &RoomId,
	alice_id: &UserId,
) -> Result {
	let state_key = format!("_{alice_id}_DEVICE_m.call");
	let path =
		format!("rooms/{room}/state/{CALL_MEMBER}/{state_key}?org.matrix.msc4140.delay={LONG}");

	let reply = alice
		.put_json(&alice.url(&path), &json!({}))
		.await?;

	let delay_id = field(&reply, "delay_id")?;

	let action = format!("delayed_events/{delay_id}/send");
	let stable = tokenless(services, &alice.client_url("v1/", &action)).await?;

	refused(stable, StatusCode::UNAUTHORIZED, "M_MISSING_TOKEN").await?;
	require(!call_member_sent(services, room, &state_key).await, "stable tokenless sent")?;

	let unstable = alice.client_url(UNSTABLE, &action);

	accepted(tokenless(services, &unstable).await?).await?;
	require(call_member_sent(services, room, &state_key).await, "the send did not land")?;

	refused(tokenless(services, &unstable).await?, StatusCode::NOT_FOUND, "M_NOT_FOUND").await
}

async fn owner_mismatch_is_not_found(
	alice: &Client<'_>,
	bob: &Client<'_>,
	room: &RoomId,
) -> Result {
	let delay_id = alice
		.schedule_dedicated(room, "m.room.message", "owner", &json!({ "body": "x" }))
		.await?;

	let url = alice.client_url(UNSTABLE, &format!("delayed_events/{delay_id}"));

	refused(bob.fetch(&url).await?, StatusCode::NOT_FOUND, "M_NOT_FOUND").await?;

	let cancel = bob
		.post_url(&format!("{url}/cancel"), &json!({}))
		.await?;

	refused(cancel, StatusCode::NOT_FOUND, "M_NOT_FOUND").await?;

	alice.act(&delay_id, "cancel").await
}

async fn transaction_retry_returns_one_delay(
	services: &Services,
	alice: &Client<'_>,
	room: &RoomId,
	alice_id: &UserId,
) -> Result {
	let content = json!({ "body": "once" });
	let first = alice
		.schedule_dedicated(room, "m.room.message", "retry", &content)
		.await?;

	// Suspension fails the message policy, which a retry must not reach.
	services
		.users
		.set_suspended(alice_id, &services.globals.server_user);

	let retry = alice
		.schedule_dedicated(room, "m.room.message", "retry", &content)
		.await;

	services.users.clear_suspended(alice_id);

	assert_eq!(retry?, first, "a retried transaction scheduled twice");

	alice.act(&first, "cancel").await
}

async fn room_auth_checked_at_schedule(bob: &Client<'_>, room: &RoomId) -> Result {
	let path = format!("rooms/{room}/state/m.room.topic?org.matrix.msc4140.delay={LONG}");
	let response = bob
		.request(Method::PUT, &bob.url(&path), &json!({ "topic": "intruder" }))
		.await?;

	refused(response, StatusCode::FORBIDDEN, "M_FORBIDDEN").await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn delay_id_reaches_only_its_sender(
	services: &Services,
	alice: &Client<'_>,
	bob: &Client<'_>,
	alice_id: &UserId,
) -> Result {
	let room = alice
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	bob.post_json(&bob.url(&format!("rooms/{room}/join")), &json!({}))
		.await?;

	let path =
		format!("rooms/{room}/state/m.room.member/{alice_id}?org.matrix.msc4140.delay=300");

	let content = json!({ "membership": "join", "displayname": "later" });
	let reply = alice
		.put_json(&alice.url(&path), &content)
		.await?;

	let delay_id = field(&reply, "delay_id")?;
	let fired = poll_until(FIRE_DEADLINE, async || {
		displayname(services, &room, alice_id)
			.await
			.is_some_and(is_equal_to!("later"))
	})
	.await;

	require(fired, "the delayed member event was never sent")?;

	let event_id = services
		.state_accessor
		.room_state_get(&room, &StateEventType::RoomMember, alice_id.as_str())
		.await?
		.event_id;

	let named = format!(r#""{DELAY_ID}":"{delay_id}""#);
	let paths = [
		format!("rooms/{room}/event/{event_id}"),
		format!("rooms/{room}/state/m.room.member/{alice_id}?format=event"),
		format!("rooms/{room}/state"),
		format!("rooms/{room}/members"),
		format!("rooms/{room}/messages?dir=b"),
	];

	for path in &paths {
		let sender = alice
			.get_json(&alice.url(path))
			.await?
			.to_string();

		let member = bob.get_json(&bob.url(path)).await?.to_string();

		assert!(sender.contains(event_id.as_str()), "{path}: {sender}");
		assert!(member.contains(event_id.as_str()), "{path}: {member}");
		assert!(sender.contains(&named), "{path}: {sender}");
		assert!(!member.contains(DELAY_ID), "{path}: {member}");
	}

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn displayname(services: &Services, room: &RoomId, user_id: &UserId) -> Option<String> {
	services
		.state_accessor
		.room_state_get_content(room, &StateEventType::RoomMember, user_id.as_str())
		.await
		.ok()
		.and_then(|content: RoomMemberEventContent| content.displayname)
}

async fn deactivation_removes_scheduled(
	services: &Services,
	bob: &Client<'_>,
	bob_id: &UserId,
) -> Result {
	let room = bob.create_room(&json!({})).await?;

	let room_lock = services.state.mutex.lock(&room).await;
	let url = bob
		.client_url(UNSTABLE, &format!("rooms/{room}/delayed_event/m.room.message/deactivate"));

	let body = json!({ "delay_ms": LONG, "content": { "body": "x" } });
	let (reply, deactivated, released) = join3(
		bob.request(Method::PUT, &url, &body),
		async {
			let held =
				poll_until(FIRE_DEADLINE, async || admission_held(services, bob_id).await).await;

			require(held, "schedule did not hold admission before deactivation")?;
			services.users.deactivate_account(bob_id).await
		},
		async {
			let held =
				poll_until(FIRE_DEADLINE, async || admission_held(services, bob_id).await).await;

			let removed = poll_until(FIRE_DEADLINE, async || {
				services
					.users
					.all_device_ids(bob_id)
					.count()
					.await
					.eq(&0)
			})
			.await;

			sleep(Duration::from_millis(250)).await;
			drop(room_lock);
			require(held && removed, "devices were not removed under admission")
		},
	)
	.await;

	assert_eq!(reply?.status(), StatusCode::OK);
	deactivated?;
	released?;

	let scheduled = services
		.delayed_events
		.scheduled(bob_id)
		.count()
		.await;

	assert_eq!(scheduled, 0, "deactivation left scheduled events behind");

	Ok(())
}

async fn admission_held(services: &Services, user_id: &UserId) -> bool {
	timeout(Duration::from_millis(50), services.delayed_events.lock_user(user_id))
		.await
		.is_err()
}

async fn limits(services: &Services, base: &str) -> Result {
	let alice_id = register(services, "alice", ALICE).await?;

	let alice = Client { services, base, token: ALICE };
	let room = alice.create_room(&json!({})).await?;
	let too_long = 120_001;

	let path =
		format!("rooms/{room}/send/m.room.message/long?org.matrix.msc4140.delay={too_long}");

	let query = alice
		.request(Method::PUT, &alice.url(&path), &json!({ "body": "x" }))
		.await?;

	refused(query, StatusCode::BAD_REQUEST, "ORG.MATRIX.MSC4140_DELAY_TOO_LARGE").await?;

	let stable = alice
		.request(
			Method::PUT,
			&alice.url(&format!("rooms/{room}/delayed_event/m.room.message/long")),
			&json!({ "delay_ms": too_long, "content": { "body": "x" } }),
		)
		.await?;

	refused(stable, StatusCode::BAD_REQUEST, "M_DELAY_TOO_LARGE").await?;

	let other_room = alice.create_room(&json!({})).await?;
	let room_lock = services.state.mutex.lock(&room).await;
	let other_lock = services.state.mutex.lock(&other_room).await;
	let first =
		alice.client_url(UNSTABLE, &format!("rooms/{room}/delayed_event/m.room.message/first"));

	let rival = alice
		.client_url(UNSTABLE, &format!("rooms/{other_room}/delayed_event/m.room.message/rival"));

	let body = json!({ "delay_ms": LONG, "content": { "body": "x" } });
	let (first, rival, released) = join3(
		alice.request(Method::PUT, &first, &body),
		alice.request(Method::PUT, &rival, &body),
		async {
			let held =
				poll_until(FIRE_DEADLINE, async || admission_held(services, &alice_id).await)
					.await;

			sleep(Duration::from_millis(250)).await;
			drop((room_lock, other_lock));
			require(held, "schedule did not hold admission before room release")
		},
	)
	.await;

	released?;

	let (first, rival) = (first?, rival?);

	assert_ne!(first.status(), rival.status(), "concurrent schedules bypassed the cap");

	let (winner, loser) = match first.status() {
		| StatusCode::OK => (first, rival),
		| _ => (rival, first),
	};

	assert_eq!(winner.status(), StatusCode::OK);
	refused(loser, StatusCode::TOO_MANY_REQUESTS, "M_LIMIT_EXCEEDED").await?;

	let scheduled = services
		.delayed_events
		.scheduled(&alice_id)
		.count()
		.await;

	assert_eq!(scheduled, 1);

	let second =
		alice.client_url(UNSTABLE, &format!("rooms/{room}/delayed_event/m.room.message/second"));

	let over = alice
		.request(Method::PUT, &second, &json!({ "delay_ms": LONG, "content": { "body": "x" } }))
		.await?;

	let retry_after = over
		.headers()
		.get(RETRY_AFTER)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse().ok());

	require(
		retry_after.is_some_and(|secs: u64| (1..=60).contains(&secs)),
		"the cap answered without a Retry-After until the next event",
	)?;

	refused(over, StatusCode::TOO_MANY_REQUESTS, "M_LIMIT_EXCEEDED").await
}

async fn disabled(services: &Services, base: &str) -> Result {
	register(services, "alice", ALICE).await?;

	let alice = Client { services, base, token: ALICE };
	let room = alice.create_room(&json!({})).await?;
	let versions = alice
		.get_json(&alice.client_url("", "versions"))
		.await?;

	let capabilities = alice.get_json(&alice.url("capabilities")).await?;

	for flag in FLAGS {
		assert_eq!(versions["unstable_features"].get(flag), None, "{flag}: {versions}");
	}

	for name in CAPABILITIES {
		assert_eq!(capabilities["capabilities"].get(name), None, "{name}: {capabilities}");
	}

	let path = format!("rooms/{room}/send/m.room.message/off?org.matrix.msc4140.delay=1000");
	let response = alice
		.request(Method::PUT, &alice.url(&path), &json!({ "body": "x" }))
		.await?;

	refused(response, StatusCode::FORBIDDEN, "M_FORBIDDEN").await
}

fn survive_a_restart() -> Result {
	let database = Args::test_database_path("delayed-events-restart");
	let outcome = run_child(&database, "schedule").and_then(|()| run_child(&database, "fire"));

	remove_dir_all(database).ok();

	outcome
}

fn run_child(database: &Path, phase: &str) -> Result {
	let output = Command::new(current_exe()?)
		.env(CHILD_DATABASE_ENV, database)
		.env(CHILD_PHASE_ENV, phase)
		.output()?;

	if !output.status.success() {
		let stdout = String::from_utf8_lossy(&output.stdout);
		let stderr = String::from_utf8_lossy(&output.stderr);

		return Err!(
			"delayed events {phase} child failed with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
			output.status,
		);
	}

	Ok(())
}

async fn schedule_before_restart(services: &Services, base: &str) -> Result {
	register(services, "alice", ALICE).await?;

	let alice = Client { services, base, token: ALICE };
	let room = alice
		.create_room(&json!({ "room_alias_name": "restart" }))
		.await?;

	let path = format!("rooms/{room}/state/m.room.topic?org.matrix.msc4140.delay=3000");
	let reply = alice
		.put_json(&alice.url(&path), &json!({ "topic": "after restart" }))
		.await?;

	let delay_id = field(&reply, "delay_id")?;

	alice.delayed(delay_id).await.map(|_| ())
}

async fn fired_after_restart(services: &Services, started: u64) -> Result {
	let alias = format!("#restart:{}", services.globals.server_name());
	let alias: &RoomAliasId = alias.as_str().try_into()?;
	let room = services.alias.resolve_local_alias(alias).await?;

	let fired = poll_until(FIRE_DEADLINE, async || {
		topic(services, &room)
			.await
			.as_deref()
			.eq(&Some("after restart"))
	})
	.await;

	require(fired, "the scheduled topic did not survive the restart")?;
	require(
		topic_sent_at(services, &room).await?.ge(&started),
		"the scheduled topic was sent before the restart",
	)
}

async fn topic(services: &Services, room: &RoomId) -> Option<String> {
	services
		.state_accessor
		.room_state_get_content(room, &StateEventType::RoomTopic, "")
		.await
		.ok()
		.map(|content: RoomTopicEventContent| content.topic)
}

async fn topic_sent_at(services: &Services, room: &RoomId) -> Result<u64> {
	let topic = services
		.state_accessor
		.room_state_get(room, &StateEventType::RoomTopic, "")
		.await?;

	let sent_at = topic.origin_server_ts().get().into();

	Ok(sent_at)
}

async fn call_member_sent(services: &Services, room: &RoomId, state_key: &str) -> bool {
	services
		.state_accessor
		.room_state_get(room, &CALL_MEMBER.into(), state_key)
		.await
		.is_ok()
}

async fn tokenless(services: &Services, url: &str) -> Result<Response> {
	let response = services
		.client
		.clients
		.default
		.post(url)
		.json(&json!({}))
		.send()
		.await?;

	Ok(response)
}

async fn refused(response: Response, status: StatusCode, errcode: &str) -> Result {
	let actual = response.status();
	let body: Value = response.json().await?;

	assert_eq!(actual, status, "{body}");
	assert_eq!(body["errcode"], errcode, "{body}");

	Ok(())
}

#[implement(Client, params = "<'_>")]
async fn get_json(&self, url: &str) -> Result<Value> { accepted(self.fetch(url).await?).await }

#[implement(Client, params = "<'_>")]
async fn fetch(&self, url: &str) -> Result<Response> {
	self.request(Method::GET, url, &json!({})).await
}

#[implement(Client, params = "<'_>")]
async fn put_json(&self, url: &str, body: &Value) -> Result<Value> {
	accepted(self.request(Method::PUT, url, body).await?).await
}

#[implement(Client, params = "<'_>")]
async fn schedule_dedicated(
	&self,
	room: &RoomId,
	event_type: &str,
	txn_id: &str,
	content: &Value,
) -> Result<String> {
	let path = format!("rooms/{room}/delayed_event/{event_type}/{txn_id}");
	let body = json!({ "delay_ms": LONG, "content": content });
	let reply = self
		.put_json(&self.client_url(UNSTABLE, &path), &body)
		.await?;

	field(&reply, "delay_id").map(ToOwned::to_owned)
}

#[implement(Client, params = "<'_>")]
async fn delayed(&self, delay_id: &str) -> Result<Value> {
	self.get_json(&self.client_url(UNSTABLE, &format!("delayed_events/{delay_id}")))
		.await
}

#[implement(Client, params = "<'_>")]
async fn act(&self, delay_id: &str, action: &str) -> Result {
	let url = self.client_url(UNSTABLE, &format!("delayed_events/{delay_id}/{action}"));

	self.post_json(&url, &json!({}))
		.await
		.map(|_| ())
		.map_err(|e| err!("{action} {delay_id}: {e}"))
}
