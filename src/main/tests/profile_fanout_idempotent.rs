#![cfg(test)]

use std::net::TcpListener;

use futures::{Stream, StreamExt, future::join};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{RoomId, UserId, profile::ProfileFieldName},
	smallvec::SmallVec,
	utils::BoolExt,
};
use tuwunel_service::{Services, profile::ProfileChange};

use self::client::{Client, register, wait_until_ready};

mod client;

// Each write in this test logs a single field.
type Fields = SmallVec<[ProfileFieldName; 1]>;

const TOKEN: &str = "profile-fanout-idempotent-test-access-token";

const MEMBER_FILTER: &str = r#"{"types":["m.room.member"]}"#;

/// Drives a global profile write that restores what every joined room already
/// holds.
///
/// Such a write is a change to nobody else, so no room gets a member event or
/// a change row for it. The writer's own devices still get a change row, and a
/// write that does change the value still gets exactly one member event.
#[test]
fn restoring_a_profile_value_emits_no_member_event() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
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

	let user_id = register(services, "fanoutowner", TOKEN).await?;
	let owner = Client { services, base, token: TOKEN };
	let room = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let joined = owner.member_events(&room).await?;

	let displayname = services.profile.displayname(&user_id).await?;
	let before = services.globals.current_count();

	owner
		.put_displayname(&user_id, &displayname)
		.await?;

	let restored = owner.member_events(&room).await?;

	BoolExt::ok_or_else(restored == joined, || {
		err!("restoring the display name grew the member events from {joined} to {restored}")
	})?;

	let changes = services
		.profile
		.profile_changed(&user_id, before, None);

	expect_logged(changes, &[ProfileFieldName::DisplayName], "restoring for the owner").await?;

	let changes = services
		.profile
		.room_profile_changed(&room, before, None);

	expect_logged(changes, &[], "restoring for the room").await?;

	let before_rename = services.globals.current_count();

	owner.put_displayname(&user_id, "renamed").await?;

	let renamed = owner.member_events(&room).await?;

	BoolExt::ok_or_else(renamed == joined.saturating_add(1), || {
		err!("changing the display name grew the member events from {joined} to {renamed}")
	})?;

	let changes = services
		.profile
		.room_profile_changed(&room, before_rename, None);

	expect_logged(changes, &[ProfileFieldName::DisplayName], "renaming for the room").await
}

/// How many member events the room's timeline holds.
///
/// One backward page of a hundred, a window rather than a room total, which
/// is plenty for a room that only ever saw one member.
#[implement(Client, params = "<'_>")]
async fn member_events(&self, room_id: &RoomId) -> Result<usize> {
	let query = [("dir", "b"), ("limit", "100"), ("filter", MEMBER_FILTER)];

	let page: Value = self
		.services
		.client
		.clients
		.default
		.get(self.url(&format!("rooms/{room_id}/messages")))
		.bearer_auth(self.token)
		.query(&query)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	page["chunk"]
		.as_array()
		.map(Vec::len)
		.ok_or_else(|| err!("messages response omitted the chunk"))
}

#[implement(Client, params = "<'_>")]
async fn put_displayname(&self, user_id: &UserId, displayname: &str) -> Result {
	self.services
		.client
		.clients
		.default
		.put(self.url(&format!("profile/{user_id}/displayname")))
		.bearer_auth(self.token)
		.json(&json!({ "displayname": displayname }))
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

async fn expect_logged(
	changes: impl Stream<Item = ProfileChange<'_>>,
	expected: &[ProfileFieldName],
	subject: &str,
) -> Result {
	let logged: Fields = changes
		.map(|(_, field)| field.into())
		.collect()
		.await;

	BoolExt::ok_or_else(logged.as_slice() == expected, || {
		err!("{subject} logged {logged:?}, expected {expected:?}")
	})
}
