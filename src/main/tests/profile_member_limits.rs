#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{OwnedRoomId, RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::client::{Client, accepted, field, register, wait_until_ready};

mod client;

#[path = "client/get.rs"]
mod get;

#[path = "client/put.rs"]
mod put;

#[path = "client/state.rs"]
mod state;

const TOKEN: &str = "profile-member-limits-test-access-token";

// The default `profile.member_event_max_bytes`.
const MEMBER_MAX: usize = 4096;

const PAD: &str = "com.example.pad";

type Expectation<'a> = (&'a str, Option<&'a Value>);

/// Local profile writes and client member events are bounded, while a
/// propagated profile change is not held to the member event cap.
///
/// Display names count characters and avatar URLs count bytes. A member event
/// over the content cap is refused when a client writes it, and a display name
/// write that pushes one room's member event over the cap still lands globally
/// and in every room.
#[test]
fn profile_and_member_writes_are_bounded() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_loopback_listener(port)
		.with_option("listening=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let driven = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (served, outcome) = join(async_run(&server), driven).await;

		drop(services);
		async_stop(&server).await?;
		served?;

		outcome
	});

	drop(runtime);

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let user_id = register(services, "memberlimits", TOKEN).await?;
	let owner = Client { services, base, token: TOKEN };

	displayname_limit(&owner, &user_id).await?;
	avatar_url_limit(&owner, &user_id).await?;
	member_event_limit(&owner, &user_id).await?;
	member_displayname_limit(&owner, &user_id).await?;
	join_extra_limit(&owner, &user_id).await?;
	create_initial_member_limit(&owner, &user_id).await?;
	padded_room_propagates(&owner, &user_id).await
}

async fn displayname_limit(owner: &Client<'_>, user_id: &UserId) -> Result {
	owner
		.put_profile(user_id, "displayname", "start")
		.await?;

	let refused = owner
		.put_profile_response(user_id, "displayname", &"x".repeat(257))
		.await?;

	expect_bad_json(refused, "257-character display name").await?;
	expect_profile(owner, user_id, "displayname", &json!("start"), "refused display name")
		.await?;

	let longest = "x".repeat(256);

	owner
		.put_profile(user_id, "displayname", &longest)
		.await?;

	expect_profile(owner, user_id, "displayname", &json!(longest), "256-character display name")
		.await
}

async fn avatar_url_limit(owner: &Client<'_>, user_id: &UserId) -> Result {
	let longest = avatar_url(984);
	let oversized = avatar_url(985);

	BoolExt::ok_or_else(longest.len() == 1000 && oversized.len() == 1001, || {
		err!("avatar URL preconditions are {} and {} bytes", longest.len(), oversized.len())
	})?;

	let refused = owner
		.put_profile_response(user_id, "avatar_url", &oversized)
		.await?;

	expect_bad_json(refused, "1001-byte avatar URL").await?;

	owner
		.put_profile(user_id, "avatar_url", &longest)
		.await?;

	expect_profile(owner, user_id, "avatar_url", &json!(longest), "1000-byte avatar URL").await
}

fn avatar_url(len: usize) -> String { format!("mxc://localhost/{}", "a".repeat(len)) }

async fn member_event_limit(owner: &Client<'_>, user_id: &UserId) -> Result {
	let room = create_public_room(owner).await?;
	let before = owner.member_content(&room, user_id).await?;
	let oversized = padded_member("join", "over", 5000);

	expect_size(&oversized, MEMBER_MAX + 1, usize::MAX, "oversized member event")?;

	let refused = owner
		.put_state_response(&room, user_id, &oversized)
		.await?;

	expect_bad_json(refused, "oversized member event").await?;

	let after = owner.member_content(&room, user_id).await?;

	BoolExt::ok_or_else(after == before, || {
		err!("refused member event: content changed to {after:?}")
	})?;

	let fitting = padded_member("join", "fits", 3950);

	expect_size(&fitting, 3900, MEMBER_MAX, "fitting member event")?;

	owner
		.put_state(&room, "m.room.member", user_id.as_str(), &fitting)
		.await?;

	expect_member(owner, &room, user_id, "fitting member event", &[(
		"displayname",
		Some(&json!("fits")),
	)])
	.await?;

	let leaving = padded_member("leave", "over", 5000);

	expect_size(&leaving, MEMBER_MAX + 1, usize::MAX, "oversized self-leave")?;

	let refused = owner
		.put_state_response(&room, user_id, &leaving)
		.await?;

	expect_bad_json(refused, "oversized self-leave").await?;

	expect_member(owner, &room, user_id, "member after refused leave", &[(
		"membership",
		Some(&json!("join")),
	)])
	.await
}

async fn member_displayname_limit(owner: &Client<'_>, user_id: &UserId) -> Result {
	let room = create_public_room(owner).await?;
	let too_long = json!({ "membership": "join", "displayname": "y".repeat(257) });
	let refused = owner
		.put_state_response(&room, user_id, &too_long)
		.await?;

	expect_bad_json(refused, "257-character member display name").await?;

	let longest = "y".repeat(256);
	let fitting = json!({ "membership": "join", "displayname": longest });

	owner
		.put_state(&room, "m.room.member", user_id.as_str(), &fitting)
		.await?;

	expect_member(owner, &room, user_id, "256-character member display name", &[(
		"displayname",
		Some(&json!(longest)),
	)])
	.await?;

	// Two bytes each, so 256 of them are 512 bytes and only a character count admits them.
	let too_wide = json!({ "membership": "join", "displayname": "é".repeat(257) });
	let refused = owner
		.put_state_response(&room, user_id, &too_wide)
		.await?;

	expect_bad_json(refused, "257 two-byte member display name").await?;

	let widest = "é".repeat(256);
	let wide = json!({ "membership": "join", "displayname": widest });

	owner
		.put_state(&room, "m.room.member", user_id.as_str(), &wide)
		.await?;

	expect_member(owner, &room, user_id, "256 two-byte member display name", &[(
		"displayname",
		Some(&json!(widest)),
	)])
	.await
}

// A join's client extras are capped alone; the profile's fields ride along unmeasured.
async fn join_extra_limit(owner: &Client<'_>, user_id: &UserId) -> Result {
	let room = create_public_room(owner).await?;

	accepted(
		owner
			.post_url(&owner.url(&format!("rooms/{room}/leave")), &json!({}))
			.await?,
	)
	.await?;

	let join_url = owner.url(&format!("rooms/{room}/join"));
	let refused = owner
		.post_url(&join_url, &json!({ (PAD): "p".repeat(5000) }))
		.await?;

	expect_bad_json(refused, "oversized join extras").await?;
	expect_member(owner, &room, user_id, "member after refused join", &[(
		"membership",
		Some(&json!("leave")),
	)])
	.await?;

	// Extras under the cap join beside a full-size profile.
	let fitting = "p".repeat(3300);

	expect_size(&json!({ (PAD): fitting }), 3000, MEMBER_MAX, "join extras")?;

	accepted(
		owner
			.post_url(&join_url, &json!({ (PAD): fitting }))
			.await?,
	)
	.await?;

	let avatar = avatar_url(984);

	expect_member(owner, &room, user_id, "fitting join extras", &[
		("membership", Some(&json!("join"))),
		(PAD, Some(&json!(fitting))),
		("avatar_url", Some(&json!(avatar))),
	])
	.await?;

	// The merged event is over the cap, so the stored avatar and name are not counted.
	let content = owner.member_content(&room, user_id).await?;

	expect_size(&content, MEMBER_MAX + 1, usize::MAX, "merged join beside a full profile")
}

// Client member events in `initial_state` are bounded before the room is created.
async fn create_initial_member_limit(owner: &Client<'_>, user_id: &UserId) -> Result {
	let create_url = owner.url("createRoom");
	let initial = |content: Value| {
		json!({
			"type": "m.room.member",
			"state_key": user_id.as_str(),
			"content": content,
		})
	};

	let oversized = json!({
		"preset": "public_chat",
		"initial_state": [initial(padded_member("join", "over", 5000))],
	});

	let rooms_before = joined_room_count(owner).await?;

	let refused = owner.post_url(&create_url, &oversized).await?;

	expect_bad_json(refused, "oversized initial member event").await?;

	let too_long = json!({
		"preset": "public_chat",
		"initial_state": [initial(json!({
			"membership": "join",
			"displayname": "z".repeat(257),
		}))],
	});

	let refused = owner.post_url(&create_url, &too_long).await?;

	expect_bad_json(refused, "257-character initial member display name").await?;

	let invalid_avatar = json!({
		"preset": "public_chat",
		"initial_state": [initial(json!({
			"membership": "join",
			"avatar_url": "mxc://localhost/caf\u{e9}",
		}))],
	});

	let refused = owner
		.post_url(&create_url, &invalid_avatar)
		.await?;

	expect_bad_json(refused, "non-ASCII initial member avatar").await?;

	BoolExt::ok_or_else(joined_room_count(owner).await? == rooms_before, || {
		err!("refused createRoom left a room behind")
	})?;

	let fitting = json!({
		"preset": "public_chat",
		"initial_state": [initial(padded_member("join", "fits", 3000))],
	});

	let created = accepted(owner.post_url(&create_url, &fitting).await?).await?;
	let room: OwnedRoomId = field(&created, "room_id")?.try_into()?;

	expect_member(owner, &room, user_id, "fitting initial member event", &[(
		"displayname",
		Some(&json!("fits")),
	)])
	.await
}

async fn joined_room_count(owner: &Client<'_>) -> Result<usize> {
	let joined = owner.get("joined_rooms").await?;

	Ok(joined
		.get("joined_rooms")
		.and_then(Value::as_array)
		.map_or(0, Vec::len))
}

async fn padded_room_propagates(owner: &Client<'_>, user_id: &UserId) -> Result {
	let padded_content = padded_member("join", "before", 3850);

	expect_size(&padded_content, 3800, MEMBER_MAX, "padded member event")?;

	let plain_content = json!({
		"membership": "join",
		"displayname": "before",
	});

	let padded = member_room(owner, user_id, &padded_content).await?;
	let plain = member_room(owner, user_id, &plain_content).await?;

	let renamed = "w".repeat(256);
	let renamed_json = json!(renamed);
	let overlaid = padded_member("join", &renamed, 3850);

	expect_size(&overlaid, MEMBER_MAX + 1, usize::MAX, "overlaid member event")?;

	owner
		.put_profile(user_id, "displayname", &renamed)
		.await?;

	expect_profile(owner, user_id, "displayname", &renamed_json, "global rename").await?;

	let propagated = owner.member_content(&padded, user_id).await?;

	expect_field(&propagated, "displayname", Some(&renamed_json), "padded room")?;
	expect_field(&propagated, PAD, padded_content.get(PAD), "padded room padding")?;
	expect_size(&propagated, MEMBER_MAX + 1, usize::MAX, "padded room after rename")?;

	expect_member(owner, &plain, user_id, "plain room", &[("displayname", Some(&renamed_json))])
		.await
}

async fn create_public_room(owner: &Client<'_>) -> Result<OwnedRoomId> {
	owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await
}

fn padded_member(membership: &str, displayname: &str, pad: usize) -> Value {
	json!({
		"membership": membership,
		"displayname": displayname,
		(PAD): "p".repeat(pad),
	})
}

async fn member_room(
	owner: &Client<'_>,
	user_id: &UserId,
	content: &Value,
) -> Result<OwnedRoomId> {
	let room = create_public_room(owner).await?;

	owner
		.put_state(&room, "m.room.member", user_id.as_str(), content)
		.await?;

	Ok(room)
}

async fn expect_member(
	owner: &Client<'_>,
	room_id: &RoomId,
	user_id: &UserId,
	subject: &str,
	expected: &[Expectation<'_>],
) -> Result {
	let content = owner.member_content(room_id, user_id).await?;

	expected
		.iter()
		.try_for_each(|(key, value)| expect_field(&content, key, *value, subject))
}

async fn expect_profile(
	owner: &Client<'_>,
	user_id: &UserId,
	field: &str,
	expected: &Value,
	subject: &str,
) -> Result {
	let content = owner
		.get(&format!("profile/{user_id}/{field}"))
		.await?;

	expect_field(&content, field, Some(expected), subject)
}

async fn expect_bad_json(response: Response, subject: &str) -> Result {
	let status = response.status();
	let body: Value = response.json().await?;
	let errcode = body.get("errcode");

	BoolExt::ok_or_else(
		status == StatusCode::BAD_REQUEST && errcode == Some(&json!("M_BAD_JSON")),
		|| err!("{subject}: answered {status} with {errcode:?}, expected 400 M_BAD_JSON"),
	)
}

// Pins a serialized size precondition, so renamed keys cannot pass the test vacuously.
fn expect_size(content: &Value, min: usize, max: usize, subject: &str) -> Result {
	let len = serde_json::to_vec(content)?.len();

	BoolExt::ok_or_else((min..=max).contains(&len), || {
		err!("{subject}: content is {len} bytes, expected {min} to {max}")
	})
}

#[implement(Client, params = "<'_>")]
async fn put_profile(&self, user_id: &UserId, field: &str, value: &str) -> Result {
	self.put_profile_response(user_id, field, value)
		.await?
		.error_for_status()?;

	Ok(())
}

#[implement(Client, params = "<'_>")]
async fn put_profile_response(
	&self,
	user_id: &UserId,
	field: &str,
	value: &str,
) -> Result<Response> {
	let url = self.url(&format!("profile/{user_id}/{field}"));

	self.request(Method::PUT, &url, &json!({ (field): value }))
		.await
}

#[implement(Client, params = "<'_>")]
async fn put_state_response(
	&self,
	room_id: &RoomId,
	user_id: &UserId,
	content: &Value,
) -> Result<Response> {
	let url = self.url(&format!("rooms/{room_id}/state/m.room.member/{user_id}"));

	self.request(Method::PUT, &url, content).await
}

#[implement(Client, params = "<'_>")]
async fn member_content(&self, room_id: &RoomId, user_id: &UserId) -> Result<Value> {
	self.get(&format!("rooms/{room_id}/state/m.room.member/{user_id}"))
		.await
}

fn expect_field(content: &Value, key: &str, expected: Option<&Value>, subject: &str) -> Result {
	let actual = content.get(key);

	BoolExt::ok_or_else(actual == expected, || {
		err!("{subject}: {key} was {actual:?}, expected {expected:?}")
	})
}
