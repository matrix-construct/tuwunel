#![cfg(test)]

use reqwest::Method;
use serde_json::json;
use tuwunel_core::{
	Err, Result,
	ruma::{OwnedUserId, UserId},
};
use tuwunel_service::Services;

use self::{
	appservice::{Bridge, register_appservice},
	client::{Client, accepted, register},
	fixture::boot,
};

mod appservice;
#[expect(
	dead_code,
	reason = "Only registration and the request helpers are shared with the client API harness."
)]
mod client;

mod fixture;

struct Field {
	name: &'static str,
	capability: &'static str,
	option: &'static str,
	value: &'static str,
}

const USER_TOKEN: &str = "profile-field-write-policy-user-token";
const ADMIN_TOKEN: &str = "profile-field-write-policy-admin-token";
const BRIDGE_TOKEN: &str = "profile-field-write-policy-bridge-token";

const BRIDGE: Bridge<'static> = Bridge {
	id: "profile-field-policy",
	token: BRIDGE_TOKEN,
	sender_localpart: "profile_bridge",
	users: "^@profile_(policy_user|bridge):.*$",
	aliases: None,
};

const DISPLAYNAME: Field = Field {
	name: "displayname",
	capability: "m.set_displayname",
	option: "profile.enable_set_displayname=false",
	value: "policy",
};

const AVATAR_URL: Field = Field {
	name: "avatar_url",
	capability: "m.set_avatar_url",
	option: "profile.enable_set_avatar_url=false",
	value: "mxc://localhost/policy",
};

#[test]
fn profile_writes_follow_the_policy() -> Result {
	boot("profile-field-enabled", ["logging.enable=false"], enabled)?;
	boot_disabled(&DISPLAYNAME, &AVATAR_URL)?;
	boot_disabled(&AVATAR_URL, &DISPLAYNAME)?;

	boot(
		"profile-field-no-writes",
		["logging.enable=false", DISPLAYNAME.option, AVATAR_URL.option],
		all_disabled,
	)
}

async fn enabled(services: &Services, base: &str) -> Result {
	let (user_id, client) = account(services, base, "profile_policy_user", USER_TOKEN).await?;

	assert_field(&client, &user_id, &DISPLAYNAME, 200).await?;
	assert_field(&client, &user_id, &AVATAR_URL, 200).await
}

fn boot_disabled(blocked: &Field, open: &Field) -> Result {
	boot(
		&format!("profile-field-no-{}", blocked.name),
		["logging.enable=false", blocked.option],
		async |services, base| disabled(services, base, blocked, open).await,
	)
}

async fn disabled(services: &Services, base: &str, blocked: &Field, open: &Field) -> Result {
	let (user_id, client) = account(services, base, "profile_policy_user", USER_TOKEN).await?;

	assert_field(&client, &user_id, blocked, 403).await?;
	assert_field(&client, &user_id, open, 200).await?;

	let (admin_id, admin) = account(services, base, "profile_policy_admin", ADMIN_TOKEN).await?;

	services.admin.make_user_admin(&admin_id).await?;

	assert_field(&admin, &admin_id, blocked, 200).await?;

	register_appservice(services, &BRIDGE).await?;

	let bridge = Client { services, base, token: BRIDGE_TOKEN };
	let masquerade = format!("{}?user_id={user_id}", field_path(&user_id, blocked));

	assert_field_at(&bridge, &masquerade, blocked, 200).await
}

async fn all_disabled(services: &Services, base: &str) -> Result {
	let (user_id, client) = account(services, base, "profile_policy_user", USER_TOKEN).await?;

	assert_field(&client, &user_id, &DISPLAYNAME, 403).await?;
	assert_field(&client, &user_id, &AVATAR_URL, 403).await
}

async fn account<'a>(
	services: &'a Services,
	base: &'a str,
	localpart: &str,
	token: &'a str,
) -> Result<(OwnedUserId, Client<'a>)> {
	let user_id = register(services, localpart, token).await?;

	Ok((user_id, Client { services, base, token }))
}

async fn assert_field(
	client: &Client<'_>,
	user_id: &UserId,
	field: &Field,
	status: u16,
) -> Result {
	assert_field_at(client, &field_path(user_id, field), field, status).await
}

fn field_path(user_id: &UserId, field: &Field) -> String {
	format!("profile/{user_id}/{}", field.name)
}

async fn assert_field_at(client: &Client<'_>, path: &str, field: &Field, status: u16) -> Result {
	assert_capability(client, field, status == 200).await?;

	let url = client.url(path);
	let body = json!({ field.name: field.value });

	let set = client.request(Method::PUT, &url, &body).await?;

	assert_status(set.status().as_u16(), status)?;

	let clear = client
		.request(Method::DELETE, &url, &json!({}))
		.await?;

	assert_status(clear.status().as_u16(), status)
}

async fn assert_capability(client: &Client<'_>, field: &Field, expected: bool) -> Result {
	let response = client
		.request(Method::GET, &client.url("capabilities"), &json!({}))
		.await?;

	let response = accepted(response).await?;
	let capabilities = &response["capabilities"];
	let enabled = capabilities[field.capability]["enabled"].as_bool();
	let disallowed = capabilities["m.profile_fields"]["disallowed"]
		.as_array()
		.is_some_and(|fields| fields.iter().any(|name| name == field.name));

	if enabled != Some(expected) || disallowed == expected {
		return Err!(
			"{} capabilities were {capabilities}, expected {} enabled={expected} and {} in \
			 m.profile_fields disallowed={}",
			field.name,
			field.capability,
			field.name,
			!expected
		);
	}

	Ok(())
}

fn assert_status(status: u16, expected: u16) -> Result {
	if status != expected {
		return Err!("status was {status}, expected {expected}");
	}

	Ok(())
}
