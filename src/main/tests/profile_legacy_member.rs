#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{RoomId, UserId, events::room::member::RoomMemberEventContent},
	utils::BoolExt,
};
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::Services;

use self::client::{Client, accepted, register, wait_until_ready};

mod client;

const TOKEN: &str = "profile-legacy-member-test-access-token";

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

	expect_refused(&owner, &room, &user_id, &long_avatar).await
}

fn member_content(displayname: &str, avatar_url: &str) -> Value {
	json!({
		"membership": "join",
		"displayname": displayname,
		"avatar_url": avatar_url,
	})
}

fn avatar_url(fill: char, len: usize) -> String {
	format!("mxc://localhost/{}", fill.to_string().repeat(len))
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

	let status = response.status();
	let body: Value = response.json().await?;
	let errcode = body.get("errcode");

	BoolExt::ok_or_else(
		status == StatusCode::BAD_REQUEST && errcode == Some(&json!("M_BAD_JSON")),
		|| err!("an over-limit member write was answered {status} with {errcode:?}"),
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
