#![cfg(test)]

use std::time::Duration;

use serde_json::json;
use tuwunel_core::{
	Result, err,
	utils::{BoolExt, future::ExtExt},
};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
	lists::{expect_count, list},
};

mod client;
mod fixture;
mod sliding_sync;

#[path = "sliding_sync/lists.rs"]
mod lists;

const NEWCOMER_TOKEN: &str = "sync-v5-empty-lists-newcomer-access-token";
const OTHER_TOKEN: &str = "sync-v5-empty-lists-other-access-token";

/// How long the idle round polls for, in milliseconds.
///
/// With nothing new for the newcomer the round must wait all of it.
const IDLE_POLL_TIMEOUT: u64 = 1_500;

/// How far short of its poll the idle round may answer, in milliseconds.
///
/// It keeps clock granularity from failing a round that did wait.
const IDLE_SLACK: u64 = 100;

/// Drives sliding sync lists that match no room, for an account with no rooms
/// and beside a list that matches one.
///
/// matrix-rust-sdk learns that a list is fully loaded only from its count, and
/// polls without waiting until then, so every list must report one, zero
/// included. A zero count must not end a long poll either, or a client that
/// waits would poll just as hard.
#[test]
fn lists_without_rooms_report_a_zero_count() -> Result {
	boot("sync-v5-empty-lists", ["logging.enable=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "emptylistsnewcomer", NEWCOMER_TOKEN).await?;
	register(services, "emptylistsother", OTHER_TOKEN).await?;

	let newcomer = Client { services, base, token: NEWCOMER_TOKEN };
	let other = Client { token: OTHER_TOKEN, ..newcomer };

	let all_rooms = list(&json!({}));
	let one_list = json!({ "lists": { "all_rooms": all_rooms } });

	let opening = newcomer.sliding_sync(None, 0, &one_list).await?;

	expect_count(&opening, "all_rooms", 0, "an account with no rooms")?;

	let private_chat = json!({ "preset": "private_chat" });

	// A room the newcomer is not in: it moves the global count and nothing else.
	other.create_room(&private_chat).await?;

	let pos = field(&opening, "pos")?;
	let (idle, waited) = newcomer
		.sliding_sync(Some(pos), IDLE_POLL_TIMEOUT, &one_list)
		.timed()
		.await;

	expect_count(&idle?, "all_rooms", 0, "the idle round")?;

	waited
		.ge(&Duration::from_millis(IDLE_POLL_TIMEOUT - IDLE_SLACK))
		.into_option()
		.ok_or_else(|| err!("the zero count ended the long poll after {waited:?}"))?;

	newcomer.create_room(&private_chat).await?;

	let invites = list(&json!({ "is_invite": true }));
	let two_lists = json!({
		"conn_id": "two-lists",
		"lists": { "all_rooms": all_rooms, "invites": invites },
	});

	let opening = newcomer.sliding_sync(None, 0, &two_lists).await?;

	expect_count(&opening, "all_rooms", 1, "a list matching the newcomer's room")?;
	expect_count(&opening, "invites", 0, "a list matching no room")
}
