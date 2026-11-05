#![cfg(test)]

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::time::sleep;
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::{
	client::{Client, field, poll_until, register},
	fixture::boot,
	require::require,
};

mod client;
mod fixture;

#[path = "client/require.rs"]
mod require;

const ALICE_TOKEN: &str = "delayed-call-membership-alice-token";
const BOB_TOKEN: &str = "delayed-call-membership-bob-token";

/// The state event type MatrixRTC clients publish their call membership in.
const CALL_MEMBER: &str = "org.matrix.msc3401.call.member";

/// How long the restarted leave waits, in milliseconds.
const DELAY: u64 = 2_000;

/// How long the client waits between restarts.
///
/// Each restart lands well before the leave is due, and three of them
/// outlast the original delay, so a leave still pending afterwards was
/// postponed by them.
const RESTART_INTERVAL: Duration = Duration::from_millis(1_200);

/// How long the leave may take to arrive once the restarts stop.
const LEAVE_DEADLINE: Duration = Duration::from_secs(10);

/// A delay no test outlives, for leaves the client cancels or sends itself.
const LONG_DELAY: u64 = 60_000;

/// A MatrixRTC client schedules its own call leave and keeps restarting it;
/// once it stops, as on a crash, the server sends the leave.
///
/// Restarts arrive in both forms: the action in the path, which
/// matrix-js-sdk calls, and in the body, which matrix-rust-sdk calls. The
/// client can also cancel the leave or send it at once, and no other user can
/// touch it. Other state, such as a room membership or an empty-key topic,
/// cannot be delayed, nor can oversized content or a suspended account's
/// membership.
#[test]
fn delayed_call_member_leave_is_sent_once_restarts_stop() -> Result {
	boot("delayed-call-membership", ["logging.enable=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let alice_id = register(services, "callalice", ALICE_TOKEN).await?;
	register(services, "callbob", BOB_TOKEN).await?;

	let alice = Client { services, base, token: ALICE_TOKEN };
	let bob = Client { token: BOB_TOKEN, ..alice };

	let room = alice.create_room(&json!({})).await?;

	let member = alice.url(&format!("rooms/{room}/state/m.room.member/{alice_id}"));
	let leave = json!({ "membership": "leave" });
	let (status, body) = call(&alice, Method::PUT, &delayed(&member, 1_000), &leave).await?;
	require(
		status == StatusCode::FORBIDDEN && body["errcode"] == "M_FORBIDDEN",
		&format!("a delayed membership was not refused: {status} {body}"),
	)?;

	let topic = alice.url(&format!("rooms/{room}/state/m.room.topic"));
	let later = json!({ "topic": "later" });
	let (status, body) = call(&alice, Method::PUT, &delayed(&topic, 1_000), &later).await?;
	require(
		status == StatusCode::FORBIDDEN,
		&format!("a delayed empty-key topic was not refused: {status} {body}"),
	)?;

	let state = alice.url(&format!("rooms/{room}/state/{CALL_MEMBER}/_{alice_id}_DEVICE"));
	let joined = json!({ "application": "m.call", "call_id": "", "device_id": "DEVICE" });
	let (status, body) = call(&alice, Method::PUT, &state, &joined).await?;
	require(status.is_success(), &format!("joining the call failed: {status} {body}"))?;

	let delay_id = schedule_leave(&alice, &state, DELAY).await?;

	sleep(RESTART_INTERVAL).await;
	let restart = update(&alice, &delay_id, "restart");
	let (status, body) = call(&alice, Method::POST, &restart, &json!({})).await?;
	require(status.is_success(), &format!("the restart failed: {status} {body}"))?;

	sleep(RESTART_INTERVAL).await;
	let in_body =
		alice.client_url("unstable/org.matrix.msc4140/", &format!("delayed_events/{delay_id}"));
	let action = json!({ "action": "restart" });
	let (status, body) = call(&alice, Method::POST, &in_body, &action).await?;
	require(status.is_success(), &format!("the restart in the body failed: {status} {body}"))?;

	sleep(RESTART_INTERVAL).await;
	let (_, current) = call(&alice, Method::GET, &state, &json!({})).await?;
	require(
		current == joined,
		&format!("the restarts did not postpone the leave: {current}"),
	)?;

	let left = poll_until(LEAVE_DEADLINE, async || {
		call(&alice, Method::GET, &state, &json!({}))
			.await
			.is_ok_and(|(_, current)| current == json!({}))
	})
	.await;
	require(left, "the delayed leave was never sent")?;

	// Both SDKs schedule a new leave when a restart finds none.
	let (status, body) = call(&alice, Method::POST, &restart, &json!({})).await?;
	require(
		status == StatusCode::NOT_FOUND && body["errcode"] == "M_NOT_FOUND",
		&format!("a restart after the leave was not M_NOT_FOUND: {status} {body}"),
	)?;

	let (status, body) = call(&alice, Method::PUT, &state, &joined).await?;
	require(status.is_success(), &format!("rejoining the call failed: {status} {body}"))?;

	let delay_id = schedule_leave(&alice, &state, LONG_DELAY).await?;
	let send = update(&bob, &delay_id, "send");
	let (status, body) = call(&bob, Method::POST, &send, &json!({})).await?;
	require(
		status == StatusCode::NOT_FOUND,
		&format!("another user sent the leave: {status} {body}"),
	)?;

	let cancel = update(&alice, &delay_id, "cancel");
	let (status, body) = call(&alice, Method::POST, &cancel, &json!({})).await?;
	require(status.is_success(), &format!("the cancel failed: {status} {body}"))?;

	let restart = update(&alice, &delay_id, "restart");
	let (status, body) = call(&alice, Method::POST, &restart, &json!({})).await?;
	require(
		status == StatusCode::NOT_FOUND,
		&format!("a cancelled leave could still be restarted: {status} {body}"),
	)?;

	let (_, current) = call(&alice, Method::GET, &state, &json!({})).await?;
	require(current == joined, &format!("a cancelled leave was sent: {current}"))?;

	// matrix-js-sdk sends the scheduled leave at once on a normal hang-up.
	let delay_id = schedule_leave(&alice, &state, LONG_DELAY).await?;
	let send = update(&alice, &delay_id, "send");
	let (status, body) = call(&alice, Method::POST, &send, &json!({})).await?;
	require(status.is_success(), &format!("the send failed: {status} {body}"))?;

	let (_, current) = call(&alice, Method::GET, &state, &json!({})).await?;
	require(current == json!({}), &format!("the send did not leave at once: {current}"))?;

	let oversized = json!({ "padding": "x".repeat(65_536) });
	let (status, body) = call(&alice, Method::PUT, &delayed(&state, 1_000), &oversized).await?;
	require(
		status == StatusCode::PAYLOAD_TOO_LARGE && body["errcode"] == "M_TOO_LARGE",
		&format!("oversized content was not refused: {status} {body}"),
	)?;

	services.users.set_suspended(&alice_id, &alice_id);

	let (status, body) = call(&alice, Method::PUT, &delayed(&state, 1_000), &json!({})).await?;
	require(
		status == StatusCode::FORBIDDEN && body["errcode"] == "M_USER_SUSPENDED",
		&format!("a suspended account scheduled a leave: {status} {body}"),
	)
}

/// Send one request as this user and return its status with its JSON body.
///
/// Refusals are part of what the test asserts, so the status stays the
/// caller's to judge.
async fn call(
	client: &Client<'_>,
	method: Method,
	url: &str,
	body: &Value,
) -> Result<(StatusCode, Value)> {
	let response = client.request(method, url, body).await?;
	let status = response.status();

	Ok((status, response.json().await?))
}

/// Schedule the call membership's leave and return its delay ID.
async fn schedule_leave(client: &Client<'_>, state: &str, millis: u64) -> Result<String> {
	let (status, body) = call(client, Method::PUT, &delayed(state, millis), &json!({})).await?;
	require(status.is_success(), &format!("scheduling the leave failed: {status} {body}"))?;

	Ok(field(&body, "delay_id")?.to_owned())
}

/// A state URL that schedules the event `millis` milliseconds ahead.
fn delayed(state: &str, millis: u64) -> String {
	format!("{state}?org.matrix.msc4140.delay={millis}")
}

/// The URL matrix-js-sdk restarts, cancels or sends a delayed event through.
fn update(client: &Client<'_>, delay_id: &str, action: &str) -> String {
	client.client_url(
		"unstable/org.matrix.msc4140/",
		&format!("delayed_events/{delay_id}/{action}"),
	)
}
