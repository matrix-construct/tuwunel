#![cfg(test)]

use std::{net::TcpListener, time::Duration};

use futures::future::join;
use quoted_printable::{ParseMode, decode};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::time::timeout;
use tuwunel_core::{
	Result, err,
	ruma::{MilliSecondsSinceUnixEpoch, RoomId, UserId, thirdparty::Medium},
	utils::{stream::ReadyExt, time::now_millis},
};
use tuwunel_database::Deserialized;
use tuwunel_service::{
	Services, pusher::EmailOutcome, sending::Destination, threepid::canonicalize_email,
};

use self::{
	client::{Client, field, poll_until, register},
	fixture::boot,
	membership::room_path,
	smtp::capture_email,
};

mod client;
mod fixture;
#[path = "client/get.rs"]
mod get;
#[path = "client/membership.rs"]
mod membership;
#[path = "client/message.rs"]
mod message;
#[path = "client/put.rs"]
mod put;
mod smtp;

const ADDRESS: &str = "Recipient@Example.com";
const HTTP_PUSHKEY: &str = "http-pushkey";
const OTHER_TOKEN: &str = "email-notifications-other-user-token";
const ROOM_NAME: &str = "Email Test Room";
const SENDER_TOKEN: &str = "email-notifications-sender-token";
const SNIPPET: &str = "<b>Fish & chips</b>";
const TOKEN: &str = "email-notifications-recipient-token";

// How long the worker and the SMTP sink each get to act.
const DEADLINE: Duration = Duration::from_secs(10);

/// Email pushers against a server with SMTP configured, in one boot.
///
/// The SMTP listener is bound before the boot, so the port the server is
/// configured with stays this test's until a case takes its connection.
/// Gateway requests may reach loopback, for an HTTP pusher beside the email
/// pusher.
#[test]
fn email_notifications() -> Result {
	let smtp = TcpListener::bind(("127.0.0.1", 0))?;
	let smtp_option = format!("smtp.connection_uri=\"smtp://{}\"", smtp.local_addr()?);
	let options = [
		smtp_option.as_str(),
		"smtp.sender=\"noreply@example.org\"",
		"well_known.client=\"https://matrix.example.org\"",
		"network.http.ip_range_denylist=[]",
	];

	boot("email-notifications", options, async move |services, base| {
		exercise(services, base, smtp).await
	})
}

async fn exercise(services: &Services, base: &str, smtp: TcpListener) -> Result {
	let user = register(services, "recipient", TOKEN).await?;
	let other = register(services, "other", OTHER_TOKEN).await?;
	let recipient = Client { services, base, token: TOKEN };
	let email_canon = canonicalize_email(ADDRESS)?;

	refused(&recipient, "not-an-address", "M_INVALID_PARAM").await?;
	refused(&recipient, ADDRESS, "M_THREEPID_NOT_FOUND").await?;

	bind_address(services, &other, &email_canon).await;
	refused(&recipient, ADDRESS, "M_THREEPID_NOT_FOUND").await?;

	services
		.threepid
		.del_binding(&other, &email_canon)
		.await;

	bind_address(services, &user, &email_canon).await;
	accept_bound(&recipient, &user).await?;

	register(services, "sender", SENDER_TOKEN).await?;

	let sender = Client { services, base, token: SENDER_TOKEN };
	let room = sender
		.create_room(&json!({ "name": ROOM_NAME, "preset": "private_chat" }))
		.await?;

	sender.admit(&room, &recipient, &user).await?;

	mail_sent(&sender, &user, &room, smtp).await?;
	read_not_mailed(&recipient, &sender, &user, &room).await?;
	http_pushed(&recipient, &sender, &user, &room).await?;
	unbound_deleted(services, &user, &email_canon).await
}

/// An email pusher the server must not store is refused with `errcode`.
///
/// An address not verified on this user's account is refused whether it is
/// bound to no one or to someone else, as Synapse refuses it, so no user can
/// direct notification mail at another's address.
async fn refused(client: &Client<'_>, pushkey: &str, errcode: &str) -> Result {
	let response = client
		.post_url(&client.url("pushers/set"), &email_pusher(pushkey))
		.await?;

	assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{pushkey} status");

	let body: Value = response.json().await?;

	assert_eq!(field(&body, "errcode")?, errcode, "{pushkey}");

	Ok(())
}

async fn bind_address(services: &Services, user: &UserId, email_canon: &str) {
	let now = MilliSecondsSinceUnixEpoch::now();

	services
		.threepid
		.put_binding(user, email_canon, Medium::Email, now, now)
		.await;
}

/// A verified address is stored, with a cursor so that only later
/// notifications are mailed.
///
/// The pushkey keeps the case the client sent, though the binding is matched
/// case-insensitively.
async fn accept_bound(client: &Client<'_>, user: &UserId) -> Result {
	client
		.post("pushers/set", &email_pusher(ADDRESS))
		.await?;

	let response = client.get("pushers").await?;
	let stored = response["pushers"]
		.as_array()
		.and_then(|pushers| pushers.first())
		.ok_or_else(|| err!("the bound email pusher was not stored"))?;

	assert_eq!(field(stored, "kind")?, "email", "stored pusher kind");
	assert_eq!(field(stored, "pushkey")?, ADDRESS, "stored pusher key");
	assert!(cursor(client.services, user).await.is_some(), "the email pusher has no cursor");

	Ok(())
}

/// A notification, once its delay elapses, is sent as one plain-text mail
/// marked auto-generated.
///
/// The evaluation runs at a time past the delay instead of waiting on the
/// worker, whose ten-minute default delay keeps it from racing this test.
async fn mail_sent(
	sender: &Client<'_>,
	user: &UserId,
	room: &RoomId,
	smtp: TcpListener,
) -> Result {
	let services = sender.services;

	sender.send_text(room, "first", SNIPPET).await?;

	let evaluation = evaluate_past_delay(services, user);
	let (outcome, message) = join(evaluation, timeout(DEADLINE, capture_email(smtp))).await;
	let message = message.map_err(|_| err!("SMTP capture timed out"))??;
	let decoded = decode(&message, ParseMode::Robust)
		.map_err(|error| err!("invalid quoted-printable email: {error}"))?;

	let text = String::from_utf8(decoded)?;
	let subject = format!("Subject: [Tuwunel Test] New messages in {ROOM_NAME}\r\n");
	let to = format!("To: {}\r\n", canonicalize_email(ADDRESS)?);

	assert_eq!(outcome, EmailOutcome::Sent, "evaluation outcome");
	assert!(message.contains(&to), "mailed to the verified form: {message}");
	assert!(message.contains(&subject), "subject: {message}");
	assert!(message.contains("Auto-Submitted: auto-generated\r\n"), "auto-submitted");
	assert!(message.contains("Content-Type: text/plain"), "plain text: {message}");
	assert!(text.contains(SNIPPET), "snippet: {text}");
	assert!(text.contains(&format!("https://matrix.to/#/{room}")), "room link: {text}");

	Ok(())
}

/// A notification read before its mail is due is never mailed.
///
/// The cursor moves past it all the same. Dropping the cursor stands in for a
/// pusher stored before email notifications existed, which has no cursor until
/// its first wake starts one. The sink took its one message, so a mail attempt
/// here fails instead of coming back idle.
async fn read_not_mailed(
	recipient: &Client<'_>,
	sender: &Client<'_>,
	user: &UserId,
	room: &RoomId,
) -> Result {
	let services = recipient.services;

	services.db["senderkey_emailcursor"].del((user, ADDRESS));

	let event_id = sender
		.send_text(room, "second", "Read before the mail")
		.await?;

	let woken = poll_until(DEADLINE, async || cursor(services, user).await.is_some()).await;

	assert!(woken, "the notification did not wake the email worker");

	let before = cursor(services, user).await;

	recipient
		.post(&room_path(room, &format!("receipt/m.read/{event_id}")), &json!({}))
		.await?;

	let outcome = evaluate_past_delay(services, user).await;
	let after = cursor(services, user).await;

	assert_eq!(outcome, EmailOutcome::Idle, "read notification outcome");
	assert!(after > before, "the cursor stayed before the read notification");

	Ok(())
}

/// A notification still reaches an HTTP pusher set beside the email pusher.
///
/// Covers routing each pusher by its kind on the append path: a push is queued
/// for the HTTP pusher, whose gateway nothing answers.
async fn http_pushed(
	recipient: &Client<'_>,
	sender: &Client<'_>,
	user: &UserId,
	room: &RoomId,
) -> Result {
	let services = recipient.services;

	recipient
		.post("pushers/set", &http_pusher("http://127.0.0.1:9/_matrix/push/v1/notify"))
		.await?;

	sender
		.send_text(room, "third", "Pushed over HTTP")
		.await?;

	let destination = Destination::Push(user.to_owned(), HTTP_PUSHKEY.to_owned());
	let pending = poll_until(DEADLINE, async || {
		let sending = &services.sending.db;
		let queued = sending
			.queued_requests(&destination)
			.ready_any(|_| true)
			.await;

		queued
			|| sending
				.active_requests_for(&destination)
				.ready_any(|_| true)
				.await
	})
	.await;

	assert!(pending, "no push was queued for the HTTP pusher");

	Ok(())
}

/// An address no longer verified on the account deletes its email pusher, and
/// with it the cursor, at the next evaluation.
///
/// Removing the binding through the service stands in for removing the
/// address.
async fn unbound_deleted(services: &Services, user: &UserId, email_canon: &str) -> Result {
	services
		.threepid
		.del_binding(user, email_canon)
		.await;

	let outcome = evaluate_past_delay(services, user).await;
	let pusher = services.pusher.get_pusher(user, ADDRESS).await;

	assert_eq!(outcome, EmailOutcome::Idle, "unbound address outcome");
	assert!(pusher.is_err(), "the email pusher outlived its address");
	assert!(cursor(services, user).await.is_none(), "the cursor outlived its pusher");

	Ok(())
}

fn email_pusher(pushkey: &str) -> Value {
	pusher("email", "m.email", pushkey, &json!({ "brand": "Tuwunel Test" }))
}

fn pusher(kind: &str, app_id: &str, pushkey: &str, data: &Value) -> Value {
	json!({
		"kind": kind,
		"app_id": app_id,
		"pushkey": pushkey,
		"app_display_name": "Tuwunel Test",
		"device_display_name": pushkey,
		"lang": "en",
		"data": data,
		"append": true,
	})
}

async fn cursor(services: &Services, user: &UserId) -> Option<u64> {
	services.db["senderkey_emailcursor"]
		.qry(&(user, ADDRESS))
		.await
		.deserialized()
		.ok()
}

async fn evaluate_past_delay(services: &Services, user: &UserId) -> EmailOutcome {
	let delay = services
		.config
		.push
		.email_delay
		.saturating_mul(1000);

	let past_delay = now_millis()
		.saturating_add(delay)
		.saturating_add(1);

	services
		.pusher
		.evaluate_email(user, ADDRESS, past_delay)
		.await
}

fn http_pusher(url: &str) -> Value {
	pusher("http", "org.example.push", HTTP_PUSHKEY, &json!({ "url": url }))
}
