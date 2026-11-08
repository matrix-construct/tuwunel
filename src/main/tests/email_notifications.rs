#![cfg(test)]

use std::net::TcpListener;

use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel_core::{
	Result, err,
	ruma::{MilliSecondsSinceUnixEpoch, UserId, thirdparty::Medium},
};
use tuwunel_service::{Services, threepid::canonicalize_email};

use self::{
	client::{Client, field, register},
	fixture::boot,
};

#[expect(
	dead_code,
	reason = "the shared client harness exposes helpers used by sibling integration tests"
)]
mod client;
mod fixture;
#[path = "client/get.rs"]
mod get;

const ADDRESS: &str = "Recipient@Example.com";
const OTHER_TOKEN: &str = "email-notifications-other-user-token";
const TOKEN: &str = "email-notifications-recipient-token";

/// Email pushers against a server with SMTP configured, in one boot.
///
/// The SMTP listener is bound for the whole boot so its port stays this
/// test's; the server connects to it only to deliver.
#[test]
fn email_notifications() -> Result {
	let smtp = TcpListener::bind(("127.0.0.1", 0))?;
	let smtp_option = format!("smtp.connection_uri=\"smtp://{}\"", smtp.local_addr()?);
	let options = [
		smtp_option.as_str(),
		"smtp.sender=\"noreply@example.org\"",
		"well_known.client=\"https://matrix.example.org\"",
	];

	boot("email-notifications", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let user = register(services, "recipient", TOKEN).await?;
	let other = register(services, "other", OTHER_TOKEN).await?;
	let client = Client { services, base, token: TOKEN };
	let email_canon = canonicalize_email(ADDRESS)?;

	refused(&client, "not-an-address", "M_INVALID_PARAM").await?;
	refused(&client, ADDRESS, "M_THREEPID_NOT_FOUND").await?;

	bind_address(services, &other, &email_canon).await;
	refused(&client, ADDRESS, "M_THREEPID_NOT_FOUND").await?;

	services
		.threepid
		.del_binding(&other, &email_canon)
		.await;

	bind_address(services, &user, &email_canon).await;
	accept_bound(&client).await
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

/// A verified address is stored.
///
/// The pushkey keeps the case the client sent, though the binding is matched
/// case-insensitively.
async fn accept_bound(client: &Client<'_>) -> Result {
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

	Ok(())
}

fn email_pusher(pushkey: &str) -> Value {
	json!({
		"kind": "email",
		"app_id": "m.email",
		"pushkey": pushkey,
		"app_display_name": "Email Notifications",
		"device_display_name": pushkey,
		"lang": "en",
		"data": { "brand": "Tuwunel Test" },
		"append": true,
	})
}
