#![cfg(test)]

use std::{
	collections::BTreeSet, env::var, fs::remove_dir_all, net::TcpListener, path::PathBuf,
	process::id as process_id, time::Duration,
};

use futures::future::join;
use serde_json::{Value, json};
use tokio::time::sleep;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err, implement,
	ruma::{RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const ALICE_TOKEN: &str = "sync-v5-list-filters-alice-access-token";
const BOB_TOKEN: &str = "sync-v5-list-filters-bob-access-token";

/// The two room tags under test, one on each room.
///
/// No `tags` filter here asks for the low-priority one, so a room carrying
/// only it must stay out of those lists, and a `not_tags` naming it must
/// exclude its room whatever else that room carries.
const FAVOURITE: &str = "m.favourite";
const LOW_PRIORITY: &str = "m.lowpriority";

/// How long the second sync polls for, in milliseconds.
const POLL_TIMEOUT: u64 = 1_500;

/// How long the `m.direct` change waits so the poll is already parked on it.
///
/// The first pass of a poll with nothing new to report finishes in
/// milliseconds, so this orders the change after it without depending on the
/// margin being large.
const SETTLE: Duration = Duration::from_millis(300);

/// Drives the sliding-sync list filters over a direct and a plain room.
///
/// One sync carries five lists whose filters sort the two rooms by `m.direct`
/// and by room tag, so each room must come back naming exactly the lists it
/// belongs to. The room payload's own `is_dm` is asserted alongside, since it
/// answers from the same source.
#[test]
fn list_filters_partition_rooms_by_dm_and_tag() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path = PathBuf::from(root).join(format!("tuwunel-sync-v5-filters-{}", process_id()));

	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.option.extend([
		format!("database_path={db_path:?}"),
		"address=[\"127.0.0.1\"]".to_owned(),
		format!("port={port}"),
		"listening=true".to_owned(),
	]);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run_result, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	drop(runtime);
	remove_dir_all(&db_path).ok();

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let alice_id = register(services, "syncvfiveralice", ALICE_TOKEN).await?;
	let bob_id = register(services, "syncvfiverbob", BOB_TOKEN).await?;

	let alice = Client { services, base, token: ALICE_TOKEN };
	let bob = Client { services, base, token: BOB_TOKEN };

	let direct_body = json!({
		"preset": "private_chat",
		"invite": [&bob_id],
		"is_direct": true,
	});

	let plain_body = json!({
		"preset": "private_chat",
		"invite": [&bob_id],
		"is_direct": false,
	});

	let direct = alice.create_room(&direct_body).await?;
	let plain = alice.create_room(&plain_body).await?;

	bob.join(&direct).await?;
	bob.join(&plain).await?;

	// The join erases the invite's `is_direct`, so account data is all that is
	// left.
	bob.mark_direct(&bob_id, &alice_id, &[&direct])
		.await?;

	bob.tag_room(&bob_id, &plain, FAVOURITE).await?;
	bob.tag_room(&bob_id, &direct, LOW_PRIORITY)
		.await?;

	let response = bob.sync_lists(None).await?;

	let matched = |room_id: &RoomId, want: &[&str]| {
		let got = matched_lists(&response, room_id);

		(got.len() == want.len() && want.iter().all(|name| got.contains(name)))
			.into_option()
			.ok_or_else(|| err!("{room_id} matched {got:?} rather than {want:?}"))
	};

	matched(&direct, &["directs", "untagged"])?;
	matched(&plain, &["favourites", "others", "priority"])?;

	is_dm(&response, &direct)
		.unwrap_or_default()
		.into_option()
		.ok_or_else(|| err!("the direct room's payload does not report itself a direct chat"))?;

	is_dm(&response, &plain)
		.unwrap_or_default()
		.is_false()
		.into_option()
		.ok_or_else(|| err!("a plain room's payload reports itself a direct chat"))?;

	woken_poll_sees_the_change(&bob, &bob_id, &alice_id, &response, &direct, &plain).await
}

/// A poll already parked must answer from `m.direct` as it stands on waking.
///
/// The set backing `is_dm` is read inside the long-poll loop rather than once
/// per request, so a change arriving mid-poll reaches the response that poll
/// returns; read before the loop it would answer stale for the request's life.
async fn woken_poll_sees_the_change(
	bob: &Client<'_>,
	bob_id: &UserId,
	alice_id: &UserId,
	response: &Value,
	direct: &RoomId,
	plain: &RoomId,
) -> Result {
	let pos = field(response, "pos")?;

	let polling = bob.sync_lists(Some(pos));
	let flipping = async {
		sleep(SETTLE).await;

		bob.mark_direct(bob_id, alice_id, &[direct, plain])
			.await
	};

	let (polled, flipped) = join(polling, flipping).await;
	flipped?;

	let count = list_count(&polled?, "directs");

	count
		.eq(&2)
		.into_option()
		.ok_or_else(|| err!("the woken poll reported {count} direct rooms rather than two"))
}

#[implement(Client, params = "<'_>")]
async fn join(&self, room_id: &RoomId) -> Result {
	self.services
		.client
		.clients
		.default
		.post(self.url(&format!("rooms/{room_id}/join")))
		.bearer_auth(self.token)
		.json(&json!({}))
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

/// Record the room as a direct chat with `counterparty`.
///
/// A real client writes `m.direct` itself when it accepts a DM invite, and
/// this is the source the server is meant to answer `is_dm` from.
#[implement(Client, params = "<'_>")]
async fn mark_direct(
	&self,
	user_id: &UserId,
	counterparty: &UserId,
	rooms: &[&RoomId],
) -> Result {
	let path = format!("user/{user_id}/account_data/m.direct");

	self.services
		.client
		.clients
		.default
		.put(self.url(&path))
		.bearer_auth(self.token)
		.json(&json!({ counterparty: rooms }))
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

#[implement(Client, params = "<'_>")]
async fn tag_room(&self, user_id: &UserId, room_id: &RoomId, tag: &str) -> Result {
	let path = format!("user/{user_id}/rooms/{room_id}/tags/{tag}");

	self.services
		.client
		.clients
		.default
		.put(self.url(&path))
		.bearer_auth(self.token)
		.json(&json!({}))
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

/// One initial sliding sync carrying every filtered list under test.
///
/// The first four come in complementary pairs, so an omission is as visible as
/// a spurious match. The fifth names a tag in both `tags` and `not_tags`, which
/// the proposal resolves in favour of `not_tags`.
#[implement(Client, params = "<'_>")]
async fn sync_lists(&self, since: Option<&str>) -> Result<Value> {
	let list = |filters: Value| {
		json!({
			"ranges": [[0, 99]],
			"required_state": [],
			"timeline_limit": 0,
			"filters": filters,
		})
	};

	let priority = json!({
		"tags": [FAVOURITE, LOW_PRIORITY],
		"not_tags": [LOW_PRIORITY],
	});

	let body = json!({
		"lists": {
			"directs": list(json!({ "is_dm": true })),
			"others": list(json!({ "is_dm": false })),
			"favourites": list(json!({ "tags": [FAVOURITE] })),
			"untagged": list(json!({ "not_tags": [FAVOURITE] })),
			"priority": list(priority),
		},
	});

	let query =
		since.map_or_else(String::new, |since| format!("?pos={since}&timeout={POLL_TIMEOUT}"));

	let url = format!(
		"{}/_matrix/client/unstable/org.matrix.simplified_msc3575/sync{query}",
		self.base
	);

	let response: Value = self
		.services
		.client
		.clients
		.default
		.post(url)
		.bearer_auth(self.token)
		.json(&body)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(response)
}

/// The number of rooms a list reported matching.
///
/// The count is recomputed on every pass from the rooms the filters admitted,
/// so it moves with `m.direct` even on a pass that re-sends no room payload.
fn list_count(response: &Value, list: &str) -> u64 {
	response
		.get("lists")
		.and_then(|lists| lists.get(list))
		.and_then(|list| list.get("count"))
		.and_then(Value::as_u64)
		.unwrap_or_default()
}

/// The lists the room came back naming.
///
/// A room the sync withheld entirely yields the empty set, which fails the
/// same comparison a wrong list membership does.
fn matched_lists<'a>(response: &'a Value, room_id: &RoomId) -> BTreeSet<&'a str> {
	room_payload(response, room_id)
		.and_then(|room| room.get("lists"))
		.and_then(Value::as_array)
		.map(|lists| lists.iter().filter_map(Value::as_str).collect())
		.unwrap_or_default()
}

/// The room payload's own direct-chat flag.
///
/// The server omits the field rather than sending `false`, so absent and
/// `false` alike mean the room is not a direct chat.
fn is_dm(response: &Value, room_id: &RoomId) -> Option<bool> {
	room_payload(response, room_id)
		.and_then(|room| room.get("is_dm"))
		.and_then(Value::as_bool)
}

fn room_payload<'a>(response: &'a Value, room_id: &RoomId) -> Option<&'a Value> {
	response
		.get("rooms")
		.and_then(|rooms| rooms.get(room_id.as_str()))
}
