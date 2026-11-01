#![cfg(test)]

use const_str::concat;
use serde_json::json;
use tuwunel_core::{Result, err, utils::BoolExt};
use tuwunel_service::Services;

use self::{
	appservice::{Bridge, register_appservice},
	client::{Client, register},
	fixture::boot,
};

mod appservice;
mod client;
mod fixture;

const MEMBER_TOKEN: &str = "appservice-reregister-member-token";
const CONFIG_ID: &str = "configured";
const CONFIG_TOKEN: &str = "appservice-reregister-config-token";
const CONFIG_OPTIONS: [&str; 2] = [
	concat!("appservice.", CONFIG_ID, ".as_token=\"", CONFIG_TOKEN, "\""),
	concat!("appservice.", CONFIG_ID, ".hs_token=\"appservice-reregister-config-hs-token\""),
];

const ORIGINAL: Bridge<'static> = Bridge {
	id: "appservice-reregister",
	token: "appservice-reregister-token",
	sender_localpart: "reregister_bridge",
	users: "^@alpha_.*:.*$",
	aliases: None,
};

const REPLACEMENT: Bridge<'static> = Bridge {
	token: "appservice-reregister-replacement-token",
	users: "^@beta_.*:.*$",
	..ORIGINAL
};

const CONFIG_ID_CLAIM: Bridge<'static> = Bridge {
	id: CONFIG_ID,
	token: "appservice-reregister-configured-token",
	sender_localpart: "reregister_configured",
	users: "^@delta_.*:.*$",
	aliases: None,
};

const OTHER: Bridge<'static> = Bridge {
	id: "appservice-reregister-other",
	token: "appservice-reregister-other-token",
	sender_localpart: "reregister_other",
	users: "^@gamma_.*:.*$",
	aliases: None,
};

#[test]
fn reregistering_an_appservice_replaces_it() -> Result {
	boot("appservice-reregister", CONFIG_OPTIONS, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "beta_member", MEMBER_TOKEN).await?;

	let member = Client { services, base, token: MEMBER_TOKEN };
	let room_id = member
		.create_room(&json!({ "preset": "private_chat" }))
		.await?;

	register_appservice(services, &ORIGINAL).await?;
	register_appservice(services, &OTHER).await?;

	let appservice = &services.appservice;
	let state_cache = &services.state_cache;
	let authenticates = async |token| {
		appservice
			.find_from_access_token(token)
			.await
			.is_ok()
	};

	let registration_info = async || {
		appservice
			.get_registration_info(ORIGINAL.id)
			.await
			.ok_or_else(|| err!("{} is not registered", ORIGINAL.id))
	};

	let before = registration_info().await?;
	let in_room = state_cache
		.appservice_in_room(&room_id, &before)
		.await;

	assert!(!in_room, "the bridge's namespace matches no member");

	register_appservice(services, &REPLACEMENT).await?;

	// Registering the same id and token again is not a token clash.
	register_appservice(services, &REPLACEMENT).await?;

	let after = registration_info().await?;
	let persisted = appservice
		.get_db_registration(ORIGINAL.id)
		.await?;

	let in_room = state_cache
		.appservice_in_room(&room_id, &after)
		.await;

	assert!(in_room, "the replacement's namespace matches the joined member");
	assert!(authenticates(ORIGINAL.token).await.is_false());
	assert!(authenticates(REPLACEMENT.token).await);
	assert_eq!(persisted.as_token, REPLACEMENT.token);

	let token_taken = Bridge { token: OTHER.token, ..REPLACEMENT };
	let refused = register_appservice(services, &token_taken).await;

	assert!(refused.is_err(), "another registration's token is refused");
	assert!(
		authenticates(REPLACEMENT.token).await,
		"a refused replacement keeps the registration"
	);

	let refused = register_appservice(services, &CONFIG_ID_CLAIM).await;

	assert!(refused.is_err(), "a configured id is refused");
	assert!(authenticates(CONFIG_TOKEN).await, "a configured appservice is not replaced");

	Ok(())
}
