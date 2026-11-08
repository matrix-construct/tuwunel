#![cfg(test)]

use std::{iter::repeat_n, net::TcpListener};

use futures::future::join;
use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{RoomId, UserId, events::room::member::RoomMemberEventContent},
	utils::BoolExt,
};
use tuwunel_database::Json;
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::Services;

use self::client::{Client, accepted, register, wait_until_ready};

mod client;

#[path = "client/get.rs"]
mod get;

const TOKEN: &str = "profile-legacy-member-test-access-token";
const INVALID_AVATAR_TOKEN: &str = "profile-legacy-invalid-avatar-test-access-token";
const JOIN_EXTRAS_TOKEN: &str = "profile-legacy-join-extras-test-access-token";
const MEMBER_ONLY_TOKEN: &str = "profile-legacy-member-only-test-access-token";
const DELAYED_TOKEN: &str = "profile-legacy-delayed-test-access-token";

// Not a valid mxc URI: the media ID is not ASCII.
const INVALID_AVATAR: &str = "mxc://localhost/caf\u{e9}";

// Not a valid mxc URI: the media ID has a percent escape.
const ESCAPED_AVATAR: &str = "mxc://localhost/a%20b";

/// A member event stored before the limits can still be re-sent, and only a new
/// over-limit value is refused.
///
/// The stored name is 300 characters and the stored avatar 1500 bytes, which the
/// limits now refuse. The event is written straight to the timeline, since a
/// client write could not store it.
#[test]
fn stored_over_limit_profile_can_be_resent_but_not_extended() -> Result {
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

	let user_id = register(services, "legacymember", TOKEN).await?;
	let owner = Client { services, base, token: TOKEN };

	let room = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let stored_name = "n".repeat(300);
	let stored = member_content(&stored_name, &avatar_url('a', 1500));

	seed_member(services, &room, &user_id, &stored).await?;

	// Re-sending the stored content writes nothing new, so it must land.
	expect_accepted(&owner, &room, &user_id, &stored).await?;

	let legal_avatar = member_content(&stored_name, &avatar_url('a', 984));

	expect_accepted(&owner, &room, &user_id, &legal_avatar).await?;

	let long_name = member_content(&"m".repeat(300), &avatar_url('a', 984));

	expect_refused(&owner, &room, &user_id, &long_name).await?;

	let long_avatar = member_content(&stored_name, &avatar_url('b', 1500));

	expect_refused(&owner, &room, &user_id, &long_avatar).await?;

	stored_invalid_avatar(services, base).await?;

	join_extras_invalid_avatar(services, base).await?;

	member_only_invalid_avatar(services, base).await?;

	delayed_invalid_avatar(services, base).await
}

/// A stored avatar that is not a valid mxc URI can be kept, but a new one is refused.
///
/// The seeded member event and profile avatar predate the format check. The new value
/// is refused on the member and profile routes, and the stored one is kept on each.
async fn stored_invalid_avatar(services: &Services, base: &str) -> Result {
	let user_id = register(services, "invalidavatar", INVALID_AVATAR_TOKEN).await?;
	let owner = Client {
		services,
		base,
		token: INVALID_AVATAR_TOKEN,
	};

	let room = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let stored = member_content("before", INVALID_AVATAR);

	// Before anything is stored, the invalid avatar is a new value, so it is refused.
	expect_refused(&owner, &room, &user_id, &stored).await?;

	let refused = put_profile_avatar(&owner, &user_id, INVALID_AVATAR).await?;

	expect_bad_json(refused, "new profile write of the invalid avatar").await?;

	seed_member(services, &room, &user_id, &stored).await?;
	seed_profile_avatar(services, &user_id, INVALID_AVATAR);

	let profile = owner.get(&format!("profile/{user_id}")).await?;

	expect_avatar(&profile, INVALID_AVATAR, "the stored profile avatar")?;

	// Re-sending the stored content, or changing only the name, leaves the avatar as stored.
	expect_accepted(&owner, &room, &user_id, &stored).await?;

	let renamed = member_content("after", INVALID_AVATAR);

	expect_accepted(&owner, &room, &user_id, &renamed).await?;

	let new_invalid = member_content("after", ESCAPED_AVATAR);

	expect_refused(&owner, &room, &user_id, &new_invalid).await?;

	let restated = put_profile_avatar(&owner, &user_id, INVALID_AVATAR).await?;

	accepted(restated).await?;

	let profile = owner.get(&format!("profile/{user_id}")).await?;

	expect_avatar(&profile, INVALID_AVATAR, "the profile avatar after a restating PUT")?;

	let refused = put_profile_avatar(&owner, &user_id, ESCAPED_AVATAR).await?;

	expect_bad_json(refused, "profile write of a new invalid avatar").await?;

	let valid = member_content("after", &avatar_url('o', 40));

	expect_accepted(&owner, &room, &user_id, &valid).await?;

	let left = owner
		.post_url(&owner.url(&format!("rooms/{room}/leave")), &json!({}))
		.await?;

	accepted(left).await?;

	let joined = owner
		.post_url(
			&owner.url(&format!("rooms/{room}/join")),
			&json!({ "avatar_url": avatar_url('p', 40) }),
		)
		.await?;

	accepted(joined).await?;

	// The stored profile avatar wins the merge, so the extras avatar is not stored.
	let member = get_member(&owner, &room, &user_id).await?;

	expect_avatar(&member, INVALID_AVATAR, "the member event after join")?;

	let cleared = put_profile_avatar(&owner, &user_id, "").await?;

	accepted(cleared).await?;

	let member = get_member(&owner, &room, &user_id).await?;

	expect_avatar(&member, "", "the member event after clearing the profile")
}

/// A join with no stored profile avatar bounds the extras avatar.
///
/// An invalid extras avatar is refused with the join. A valid one is stored on the
/// member event.
async fn join_extras_invalid_avatar(services: &Services, base: &str) -> Result {
	let user_id = register(services, "joinextras", JOIN_EXTRAS_TOKEN).await?;
	let owner = Client { services, base, token: JOIN_EXTRAS_TOKEN };

	let room = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let left = owner
		.post_url(&owner.url(&format!("rooms/{room}/leave")), &json!({}))
		.await?;

	accepted(left).await?;

	let join_url = owner.url(&format!("rooms/{room}/join"));
	let refused = owner
		.post_url(&join_url, &json!({ "avatar_url": ESCAPED_AVATAR }))
		.await?;

	expect_bad_json(refused, "join with an invalid extras avatar").await?;

	let valid = avatar_url('q', 40);
	let joined = owner
		.post_url(&join_url, &json!({ "avatar_url": valid }))
		.await?;

	accepted(joined).await?;

	let member = get_member(&owner, &room, &user_id).await?;

	expect_avatar(&member, &valid, "the member event after a valid extras avatar")
}

/// A member event alone can hold an invalid avatar and keep it.
///
/// No profile avatar is stored, so the restated write can only be accepted because it
/// matches the member event. A different invalid avatar is still refused.
async fn member_only_invalid_avatar(services: &Services, base: &str) -> Result {
	let user_id = register(services, "memberonly", MEMBER_ONLY_TOKEN).await?;
	let owner = Client { services, base, token: MEMBER_ONLY_TOKEN };

	let room = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	seed_member(services, &room, &user_id, &member_content("before", INVALID_AVATAR)).await?;

	let restated = member_content("after", INVALID_AVATAR);

	expect_accepted(&owner, &room, &user_id, &restated).await?;

	let member = get_member(&owner, &room, &user_id).await?;

	expect_avatar(&member, INVALID_AVATAR, "the member event after restating")?;

	let new_invalid = member_content("after", ESCAPED_AVATAR);

	expect_refused(&owner, &room, &user_id, &new_invalid).await
}

/// A delayed member event is held to the member limits when it is scheduled.
///
/// A refusal at scheduling leaves nothing to be sent when the delay ends, and a valid
/// event is still scheduled.
async fn delayed_invalid_avatar(services: &Services, base: &str) -> Result {
	let user_id = register(services, "delayedmember", DELAYED_TOKEN).await?;
	let owner = Client { services, base, token: DELAYED_TOKEN };

	let room = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let url = owner.url(&format!(
		"rooms/{room}/state/m.room.member/{user_id}?org.matrix.msc4140.delay=60000"
	));

	let invalid = member_content("after", ESCAPED_AVATAR);
	let refused = owner.request(Method::PUT, &url, &invalid).await?;

	expect_bad_json(refused, "delayed member event with an invalid avatar").await?;

	let valid = member_content("after", &avatar_url('r', 40));
	let scheduled = owner.request(Method::PUT, &url, &valid).await?;

	accepted(scheduled).await?;

	Ok(())
}

fn member_content(displayname: &str, avatar_url: &str) -> Value {
	json!({
		"membership": "join",
		"displayname": displayname,
		"avatar_url": avatar_url,
	})
}

fn avatar_url(fill: char, len: usize) -> String {
	format!("mxc://localhost/{}", repeat_n(fill, len).collect::<String>())
}

async fn seed_member(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	content: &Value,
) -> Result {
	let content: RoomMemberEventContent = serde_json::from_value(content.clone())?;
	let builder = PduBuilder::state(user_id.as_str(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(builder, user_id, room_id, &state_lock)
		.await?;

	Ok(())
}

// Seeds a profile avatar without the write checks, as a value stored before them.
fn seed_profile_avatar(services: &Services, user_id: &UserId, avatar: &str) {
	services.db["useridprofilekey_value"].put((user_id, "avatar_url"), Json(avatar));
}

async fn expect_accepted(
	owner: &Client<'_>,
	room_id: &RoomId,
	user_id: &UserId,
	content: &Value,
) -> Result {
	let response = owner
		.put_state_response(room_id, user_id, content)
		.await?;

	accepted(response).await?;

	Ok(())
}

async fn expect_refused(
	owner: &Client<'_>,
	room_id: &RoomId,
	user_id: &UserId,
	content: &Value,
) -> Result {
	let response = owner
		.put_state_response(room_id, user_id, content)
		.await?;

	expect_bad_json(response, "refused member write").await
}

async fn put_profile_avatar(
	owner: &Client<'_>,
	user_id: &UserId,
	avatar: &str,
) -> Result<Response> {
	let url = owner.url(&format!("profile/{user_id}/avatar_url"));

	owner
		.request(Method::PUT, &url, &json!({ "avatar_url": avatar }))
		.await
}

async fn get_member(owner: &Client<'_>, room_id: &RoomId, user_id: &UserId) -> Result<Value> {
	owner
		.get(&format!("rooms/{room_id}/state/m.room.member/{user_id}"))
		.await
}

fn expect_avatar(content: &Value, avatar: &str, subject: &str) -> Result {
	BoolExt::ok_or_else(content["avatar_url"] == avatar, || {
		err!("{subject}: avatar_url was {}, expected {avatar}", content["avatar_url"])
	})
}

async fn expect_bad_json(response: Response, subject: &str) -> Result {
	let status = response.status();
	let body: Value = response.json().await?;
	let errcode = body.get("errcode").and_then(Value::as_str);

	BoolExt::ok_or_else(
		status == StatusCode::BAD_REQUEST && errcode == Some("M_BAD_JSON"),
		|| err!("{subject} was answered {status} with {errcode:?}, expected 400 M_BAD_JSON"),
	)
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
