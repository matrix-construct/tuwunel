#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

mod client;
mod fixture;

const OWNER_TOKEN: &str = "state-member-banned-room-owner-token";
const USER_TOKEN: &str = "state-member-banned-room-user-token";

/// A join sent as a member state event is refused in a banned room.
///
/// `/join` refuses a room the server banned, so the same membership sent
/// through the state endpoint must not let a non-admin in either.
#[test]
fn state_member_join_refused_in_banned_room() -> Result {
	let options: [&str; 0] = [];

	boot("state-member-banned-room", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "bannedroomowner", OWNER_TOKEN).await?;

	let user_id = register(services, "bannedroomuser", USER_TOKEN).await?;
	let owner = Client { services, base, token: OWNER_TOKEN };
	let user = Client { services, base, token: USER_TOKEN };
	let room_id = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	services.metadata.ban_room(&room_id);

	let response = services
		.client
		.clients
		.default
		.put(user.url(&format!("rooms/{room_id}/state/m.room.member/{user_id}")))
		.bearer_auth(user.token)
		.json(&json!({ "membership": "join" }))
		.send()
		.await?;

	assert_eq!(response.status(), 403);
	assert_eq!(response.json::<Value>().await?["errcode"], "M_FORBIDDEN");
	assert!(
		!services
			.state_cache
			.is_joined(&user_id, &room_id)
			.await
	);

	Ok(())
}
