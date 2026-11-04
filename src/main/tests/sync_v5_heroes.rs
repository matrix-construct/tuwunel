#![cfg(test)]

use serde_json::json;
use tuwunel_core::{Result, ruma::OwnedMxcUri};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
	lists::{expect_count, list},
};

mod client;
mod fixture;
mod sliding_sync;

#[path = "sliding_sync/lists.rs"]
mod lists;

#[expect(dead_code)] // Only the self-membership action is used here.
#[path = "client/membership.rs"]
mod membership;

const OWNER_TOKEN: &str = "sync-v5-heroes-owner-access-token";
const ROOM_NAME: &str = "A named room";

// The server's limit, restated so that changing it fails this test.
const MAX_HEROES: usize = 5;

// In reverse sort order, so that the order they join in cannot pass for member order.
const MEMBERS: [(&str, &str); 6] = [
	("heroesmember6", "sync-v5-heroes-member-6-access-token"),
	("heroesmember5", "sync-v5-heroes-member-5-access-token"),
	("heroesmember4", "sync-v5-heroes-member-4-access-token"),
	("heroesmember3", "sync-v5-heroes-member-3-access-token"),
	("heroesmember2", "sync-v5-heroes-member-2-access-token"),
	("heroesmember1", "sync-v5-heroes-member-1-access-token"),
];

/// Drives room heroes through one sliding sync list over an unnamed room and a
/// named one.
///
/// A room with no name gets its five lowest other members, whatever order they
/// joined in, and the first one's avatar. A room with a name sends an empty
/// list and takes neither a computed name nor an avatar from its members.
#[test]
fn heroes_follow_member_order_and_skip_named_rooms() -> Result {
	boot("sync-v5-heroes", ["logging.enable=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "heroesanowner", OWNER_TOKEN).await?;

	let owner = Client { services, base, token: OWNER_TOKEN };
	let unnamed = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let named = owner
		.create_room(&json!({ "preset": "public_chat", "name": ROOM_NAME }))
		.await?;

	let server_name = services.globals.server_name();
	let avatar = |localpart: &str| OwnedMxcUri::from(format!("mxc://{server_name}/{localpart}"));

	for (localpart, token) in MEMBERS {
		let user_id = register(services, localpart, token).await?;

		services
			.profile
			.set_avatar_url(&user_id, Some(&avatar(localpart)), None)
			.await?;

		let member = Client { token, ..owner };

		member.act(&unnamed, "join").await?;
		member.act(&named, "join").await?;
	}

	let body = json!({ "lists": { "all_rooms": list(&json!({})) } });
	let response = owner.sliding_sync(None, 0, &body).await?;

	expect_count(&response, "all_rooms", 2, "the opening round")?;

	let rooms = &response["rooms"];
	let [.., (first_hero, _)] = MEMBERS;
	let expected: Vec<_> = MEMBERS
		.iter()
		.rev()
		.take(MAX_HEROES)
		.map(|(localpart, _)| format!("@{localpart}:{server_name}"))
		.collect();

	let unnamed = &rooms[unnamed.as_str()];
	let heroes: Vec<_> = unnamed["heroes"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(|hero| hero["user_id"].as_str())
		.collect();

	assert_eq!(heroes, expected, "an unnamed room leads with its lowest other members");
	assert!(unnamed["name"].is_string(), "an unnamed room gets a computed name");
	assert_eq!(
		unnamed["avatar"],
		avatar(first_hero).as_str(),
		"an unnamed room takes its first hero's avatar"
	);

	let named = &rooms[named.as_str()];

	assert_eq!(named["heroes"], json!([]), "a named room sends an empty hero list");
	assert_eq!(named["name"], ROOM_NAME, "a named room keeps its own name");
	assert!(named["avatar"].is_null(), "a named room borrows no hero avatar");

	Ok(())
}
