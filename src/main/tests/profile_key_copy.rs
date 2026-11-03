#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use reqwest::{Method, RequestBuilder};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{OwnedRoomId, RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::client::{Client, register, wait_until_ready};

mod client;

#[path = "client/get.rs"]
mod get;

#[path = "client/put.rs"]
mod put;

#[path = "client/state.rs"]
mod state;

const TOKEN: &str = "profile-key-copy-test-access-token";

const PROPAGATE_TO: &str = "computer.gingershaped.msc4466.propagate_to";

type Expectation<'a> = (&'a str, Option<&'a Value>);

/// A profile write copies the previous member event's keys, and the MSC4466
/// `unchanged` axis still leaves a room override alone.
///
/// The rooms hold three shapes of previous member event: unknown keys with a
/// `reason`, an empty `avatar_url`, and a per-room override. A write under
/// `unchanged` keeps the override and still updates the room whose name matches
/// the global profile.
#[test]
fn profile_writes_copy_member_content_per_msc4466() -> Result {
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

	let user_id = register(services, "keycopy", TOKEN).await?;
	let owner = Client { services, base, token: TOKEN };
	let flag = json!({ "a": [1] });

	let unknown_member = json!({
		"membership": "join",
		"displayname": "start",
		"reason": "why",
		"com.example.flag": flag,
	});

	let unknown = member_room(&owner, &user_id, &unknown_member).await?;

	expect_member(&owner, &unknown, &user_id, "precondition", &[("reason", Some(&json!("why")))])
		.await?;

	let empty_avatar_member = json!({
		"membership": "join",
		"displayname": "start",
		"avatar_url": "",
	});

	let empty_avatar = member_room(&owner, &user_id, &empty_avatar_member).await?;

	owner
		.put_profile(&user_id, "displayname", "renamed", None)
		.await?;

	expect_member(&owner, &unknown, &user_id, "unknown keys", &[
		("membership", Some(&json!("join"))),
		("displayname", Some(&json!("renamed"))),
		("com.example.flag", Some(&flag)),
		("reason", None),
	])
	.await?;

	expect_member(&owner, &empty_avatar, &user_id, "empty avatar", &[
		("displayname", Some(&json!("renamed"))),
		("avatar_url", Some(&json!(""))),
	])
	.await?;

	// Created after the global rename, so the rename cannot overwrite it.
	let overridden_member = json!({
		"membership": "join",
		"displayname": "per-room",
	});

	let overridden = member_room(&owner, &user_id, &overridden_member).await?;

	owner
		.put_profile(&user_id, "displayname", "global-2", Some("unchanged"))
		.await?;

	expect_member(&owner, &overridden, &user_id, "unchanged override", &[(
		"displayname",
		Some(&json!("per-room")),
	)])
	.await?;

	expect_member(&owner, &unknown, &user_id, "unchanged match", &[
		("displayname", Some(&json!("global-2"))),
		("com.example.flag", Some(&flag)),
	])
	.await?;

	owner
		.delete_profile(&user_id, "displayname", Some("unchanged"))
		.await?;

	expect_member(&owner, &overridden, &user_id, "unchanged delete override", &[(
		"displayname",
		Some(&json!("per-room")),
	)])
	.await?;

	expect_member(&owner, &unknown, &user_id, "unchanged delete match", &[("displayname", None)])
		.await
}

async fn member_room(
	owner: &Client<'_>,
	user_id: &UserId,
	content: &Value,
) -> Result<OwnedRoomId> {
	let room = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

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

#[implement(Client, params = "<'_>")]
async fn put_profile(
	&self,
	user_id: &UserId,
	field: &str,
	value: &str,
	propagate_to: Option<&str>,
) -> Result {
	let body = json!({ (field): value });

	self.profile_request(Method::PUT, user_id, field, Some(&body), propagate_to)
		.await
}

#[implement(Client, params = "<'_>")]
async fn delete_profile(
	&self,
	user_id: &UserId,
	field: &str,
	propagate_to: Option<&str>,
) -> Result {
	self.profile_request(Method::DELETE, user_id, field, None, propagate_to)
		.await
}

#[implement(Client, params = "<'_>")]
async fn profile_request(
	&self,
	method: Method,
	user_id: &UserId,
	field: &str,
	body: Option<&Value>,
	propagate_to: Option<&str>,
) -> Result {
	let query = propagate_to.map(|mode| (PROPAGATE_TO, mode));

	let request = self
		.services
		.client
		.clients
		.default
		.request(method, self.url(&format!("profile/{user_id}/{field}")))
		.bearer_auth(self.token)
		.query(query.as_slice());

	body.into_iter()
		.fold(request, RequestBuilder::json)
		.send()
		.await?
		.error_for_status()?;

	Ok(())
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
