#![cfg(test)]

use std::time::{Duration, Instant};

use futures::future::join;
use serde_json::{Value, json};
use tokio::time::sleep;
use tuwunel_core::{Result, err, implement, utils::BoolExt};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
};

mod client;
mod fixture;

const READER_TOKEN: &str = "sync-v5-fallback-key-wakeups-reader-token";
const OTHER_TOKEN: &str = "sync-v5-fallback-key-wakeups-other-token";

/// How long the idle round polls for, in milliseconds.
///
/// With nothing new for the reader the round must wait all of it.
const IDLE_POLL_TIMEOUT: u64 = 1_500;

/// How far short of its poll the idle round may answer, in milliseconds.
///
/// It keeps clock granularity from failing a round that did wait.
const IDLE_SLACK: u64 = 100;

/// How long the round with news polls for, in milliseconds.
///
/// It must answer on its first pass, so the server's budget is set far above
/// the deadline the test gives the whole request.
const NEWS_POLL_TIMEOUT: u64 = 30_000;

/// How long the round with news may take from request to parsed response.
///
/// It sits far below the round's poll budget, so a round that waited out its
/// poll fails here.
const NEWS_DEADLINE: Duration = Duration::from_secs(10);

/// How long the reader waits before acting during the woken round.
///
/// It lets the poll register its watchers first, so the activity wakes a
/// waiting poll rather than landing before its first pass.
const OWN_ACTIVITY_DELAY: Duration = Duration::from_millis(500);

/// Drives long polls carrying the e2ee extension past unrelated, then own,
/// activity.
///
/// The extension reports the device's unused fallback key types on every
/// pass, and activity elsewhere moves the global count, so the poll's first
/// pass runs and must keep waiting rather than answer with the types alone.
/// A poll after the reader joins a room must answer well before its timeout,
/// so the first wait cannot come from a server that never answers. A poll on
/// a connection asking for the extension alone must answer once the reader's
/// own activity wakes it, since matrix-rust-sdk backs up new keys on that
/// answer.
#[test]
fn fallback_key_types_alone_do_not_end_a_long_poll() -> Result {
	boot("sync-v5-fallback-key-wakeups", ["log_enable=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "fallbackreader", READER_TOKEN).await?;
	register(services, "fallbackother", OTHER_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let other = Client { token: OTHER_TOKEN, ..reader };

	let body = json!({
		"lists": {
			"all": {
				"ranges": [[0, 9]],
				"timeline_limit": 0,
			},
		},
		"extensions": {
			"e2ee": { "enabled": true },
		},
	});

	let opening = reader.sync_e2ee(&body, None).await?;
	let pos = field(&opening, "pos")?;

	let private_chat = json!({ "preset": "private_chat" });

	// A room the reader is not in: it moves the global count and nothing else.
	other.create_room(&private_chat).await?;

	let started = Instant::now();
	let idle = reader
		.sync_e2ee(&body, Some((pos, IDLE_POLL_TIMEOUT)))
		.await?;

	let waited = started.elapsed();

	idle["extensions"]["e2ee"]["device_unused_fallback_key_types"]
		.is_array()
		.into_option()
		.ok_or_else(|| err!("the idle round did not report the fallback key types"))?;

	waited
		.ge(&Duration::from_millis(IDLE_POLL_TIMEOUT - IDLE_SLACK))
		.into_option()
		.ok_or_else(|| {
			err!("the fallback key types alone ended the long poll after {waited:?}")
		})?;

	let pos = field(&idle, "pos")?;
	let room = reader.create_room(&private_chat).await?;

	let started = Instant::now();
	let news = reader
		.sync_e2ee(&body, Some((pos, NEWS_POLL_TIMEOUT)))
		.await?;

	let waited = started.elapsed();

	news["rooms"][room.as_str()]
		.is_object()
		.into_option()
		.ok_or_else(|| err!("the round with news did not carry the new room"))?;

	answered_before_deadline(waited, "a round with news")?;

	let extension_only = json!({
		"conn_id": "extension-only",
		"extensions": {
			"e2ee": { "enabled": true },
		},
	});

	let opening = reader.sync_e2ee(&extension_only, None).await?;
	let pos = field(&opening, "pos")?;

	let started = Instant::now();
	let poll = reader.sync_e2ee(&extension_only, Some((pos, NEWS_POLL_TIMEOUT)));

	let own_activity = async {
		sleep(OWN_ACTIVITY_DELAY).await;
		reader.create_room(&private_chat).await
	};

	let (woken, own_activity) = join(poll, own_activity).await;
	let waited = started.elapsed();

	own_activity?;
	let woken = woken?;

	woken["extensions"]["e2ee"]["device_unused_fallback_key_types"]
		.is_array()
		.into_option()
		.ok_or_else(|| err!("the woken round did not report the fallback key types"))?;

	waited
		.ge(&OWN_ACTIVITY_DELAY)
		.into_option()
		.ok_or_else(|| err!("the woken round answered before the reader acted ({waited:?})"))?;

	answered_before_deadline(waited, "the woken round")
}

/// One sliding sync whose body carries the e2ee extension.
///
/// Without a resume position it opens the connection and answers at once;
/// with one it long-polls for the given number of milliseconds.
#[implement(Client, params = "<'_>")]
async fn sync_e2ee(&self, body: &Value, resume: Option<(&str, u64)>) -> Result<Value> {
	let query =
		resume.map_or_else(String::new, |(pos, timeout)| format!("?pos={pos}&timeout={timeout}"));

	let url = format!(
		"{}/_matrix/client/unstable/org.matrix.simplified_msc3575/sync{query}",
		self.base
	);

	let response = self
		.post_url(&url, body)
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(response)
}

/// Fails a round expected to answer that waited out the deadline instead.
fn answered_before_deadline(waited: Duration, round: &str) -> Result {
	waited
		.lt(&NEWS_DEADLINE)
		.into_option()
		.ok_or_else(|| err!("{round} waited for the poll ({waited:?})"))
}
