#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{Err, Result, err, ruma::UserId};
use tuwunel_service::Services;

use self::{
	appservice::{Bridge, register_appservice},
	client::{Client, register},
	fixture::boot,
};

mod appservice;
#[expect(
	dead_code,
	reason = "Only registration and request URLs are shared with the client API harness."
)]
mod client;

mod fixture;

const USER_TOKEN: &str = "profile-displayname-policy-user-token";
const ADMIN_TOKEN: &str = "profile-displayname-policy-admin-token";
const BRIDGE_TOKEN: &str = "profile-displayname-policy-bridge-token";

const BRIDGE: Bridge<'static> = Bridge {
	id: "profile-displayname-policy",
	token: BRIDGE_TOKEN,
	sender_localpart: "profile_bridge",
	users: "^@profile_(policy_user|bridge):.*$",
	aliases: None,
};

#[test]
fn displayname_writes_follow_the_policy() -> Result {
	boot("profile-displayname-enabled", ["log_enable=false"], enabled).and_then(|()| {
		boot(
			"profile-displayname-disabled",
			["log_enable=false", "enable_set_displayname=false"],
			disabled,
		)
	})
}

async fn enabled(services: &Services, base: &str) -> Result {
	let user_id = register(services, "profile_policy_user", USER_TOKEN).await?;
	let client = Client { services, base, token: USER_TOKEN };

	assert_displayname_capability(&client, true).await?;
	assert_status(set_displayname(&client, &user_id, "enabled").await?, 200)?;
	assert_status(clear_displayname(&client, &user_id).await?, 200)
}

async fn disabled(services: &Services, base: &str) -> Result {
	let user_id = register(services, "profile_policy_user", USER_TOKEN).await?;
	let client = Client { services, base, token: USER_TOKEN };

	assert_displayname_capability(&client, false).await?;
	assert_status(set_displayname(&client, &user_id, "blocked").await?, 403)?;
	assert_status(clear_displayname(&client, &user_id).await?, 403)?;

	let admin_id = register(services, "profile_policy_admin", ADMIN_TOKEN).await?;

	services.admin.make_user_admin(&admin_id).await?;

	let admin = Client { services, base, token: ADMIN_TOKEN };

	assert_displayname_capability(&admin, true).await?;
	assert_status(set_displayname(&admin, &admin_id, "exempt").await?, 200)?;
	assert_status(clear_displayname(&admin, &admin_id).await?, 200)?;

	register_appservice(services, &BRIDGE).await?;

	let bridge = Client { services, base, token: BRIDGE_TOKEN };

	assert_displayname_capability(&bridge, true).await?;
	assert_status(set_displayname_as(&bridge, &user_id, "synchronized").await?, 200)?;
	assert_status(clear_displayname_as(&bridge, &user_id).await?, 200)
}

async fn assert_displayname_capability(client: &Client<'_>, expected: bool) -> Result {
	let response: Value = client
		.services
		.client
		.clients
		.default
		.get(client.url("capabilities"))
		.bearer_auth(client.token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	let capabilities = &response["capabilities"];
	let enabled = capabilities["m.set_displayname"]["enabled"].as_bool();
	let disallowed = capabilities["m.profile_fields"]["disallowed"]
		.as_array()
		.is_some_and(|fields| fields.iter().any(|field| field == "displayname"));

	if enabled != Some(expected) || disallowed == expected {
		return Err!(
			"displayname capabilities were {capabilities}, expected m.set_displayname \
			 enabled={expected} and displayname in m.profile_fields disallowed={}",
			!expected
		);
	}

	Ok(())
}

async fn set_displayname(
	client: &Client<'_>,
	user_id: &UserId,
	displayname: &str,
) -> Result<u16> {
	set_displayname_at(
		client,
		&client.url(&format!("profile/{user_id}/displayname")),
		displayname,
	)
	.await
}

async fn set_displayname_as(
	client: &Client<'_>,
	user_id: &UserId,
	displayname: &str,
) -> Result<u16> {
	set_displayname_at(
		client,
		&format!("{}?user_id={user_id}", client.url(&format!("profile/{user_id}/displayname"))),
		displayname,
	)
	.await
}

async fn set_displayname_at(client: &Client<'_>, url: &str, displayname: &str) -> Result<u16> {
	Ok(client
		.services
		.client
		.clients
		.default
		.put(url)
		.bearer_auth(client.token)
		.json(&json!({ "displayname": displayname }))
		.send()
		.await?
		.status()
		.as_u16())
}

async fn clear_displayname(client: &Client<'_>, user_id: &UserId) -> Result<u16> {
	clear_displayname_at(client, &client.url(&format!("profile/{user_id}/displayname"))).await
}

async fn clear_displayname_as(client: &Client<'_>, user_id: &UserId) -> Result<u16> {
	clear_displayname_at(
		client,
		&format!("{}?user_id={user_id}", client.url(&format!("profile/{user_id}/displayname"))),
	)
	.await
}

async fn clear_displayname_at(client: &Client<'_>, url: &str) -> Result<u16> {
	Ok(client
		.services
		.client
		.clients
		.default
		.delete(url)
		.bearer_auth(client.token)
		.send()
		.await?
		.status()
		.as_u16())
}

fn assert_status(status: u16, expected: u16) -> Result {
	(status == expected)
		.then_some(())
		.ok_or_else(|| err!("status was {status}, expected {expected}"))
}
