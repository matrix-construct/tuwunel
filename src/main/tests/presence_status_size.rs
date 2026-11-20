#![cfg(test)]

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{UInt, presence::PresenceState, user_id},
};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

#[expect(
	dead_code,
	reason = "Only registration and requests are used from the client harness."
)]
mod client;
mod fixture;

#[test]
fn presence_status_is_held_to_4_kib() -> Result {
	boot(
		"presence-status-size",
		["logging.enable=false", "presence.allow_local=true"],
		exercise,
	)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let token = "presence-status-size-test-access-token";
	let local = register(services, "local", token).await?;
	let client = Client { services, base, token };
	let url = client.url(&format!("presence/{local}/status"));
	let at_limit = "é".repeat(2048);
	let too_long = format!("{at_limit}!");
	let set = async |status: &str| {
		client
			.request(Method::PUT, &url, &json!({"presence": "online", "status_msg": status}))
			.await
	};

	assert_eq!(set(&at_limit).await?.status(), StatusCode::OK);
	let refused = set(&too_long).await?;
	assert_eq!(refused.status(), StatusCode::PAYLOAD_TOO_LARGE);
	assert_eq!(refused.json::<Value>().await?["errcode"], "M_TOO_LARGE");
	let stored = services.presence.get_presence(&local).await?;
	assert_eq!(stored.content.status_msg.as_ref(), Some(&at_limit));

	let remote = user_id!("@remote:example.org");
	let federate = async |state: PresenceState, status: String| {
		services
			.presence
			.set_presence_from_federation(remote, &state, false, UInt::MIN, Some(status))
			.await?;
		services.presence.get_presence(remote).await
	};
	let stored = federate(PresenceState::Online, at_limit.clone()).await?;
	assert_eq!(stored.content.status_msg, Some(at_limit));
	let stored = federate(PresenceState::Unavailable, too_long).await?;
	assert_eq!(stored.content.presence, PresenceState::Unavailable);
	assert_eq!(stored.content.status_msg, None);
	Ok(())
}
