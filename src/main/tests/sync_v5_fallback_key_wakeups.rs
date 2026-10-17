#![cfg(test)]

use std::time::{Duration, Instant};

use serde_json::{Value, json};
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

/// Drives a long poll carrying the e2ee extension past unrelated activity.
///
/// The extension reports the device's unused fallback key types on every
/// pass. Activity elsewhere on the server moves the global count, so the
/// poll's first pass runs; with nothing new for the reader it must keep
/// waiting rather than answer with the fallback key types alone. A second
/// poll after the reader joins a room must answer well before its timeout, so
/// the first wait cannot come from a server that never answers.
#[test]
fn fallback_key_types_alone_do_not_end_a_long_poll() -> Result {
	boot("sync-v5-fallback-key-wakeups", ["log_enable=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "fallbackreader", READER_TOKEN).await?;
	register(services, "fallbackother", OTHER_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let other = Client { token: OTHER_TOKEN, ..reader };

	let opening = reader.sync_e2ee(None).await?;
	let pos = field(&opening, "pos")?;

	let private_chat = json!({ "preset": "private_chat" });

	// A room the reader is not in: it moves the global count and nothing else.
	other.create_room(&private_chat).await?;

	let started = Instant::now();
	let idle = reader
		.sync_e2ee(Some((pos, IDLE_POLL_TIMEOUT)))
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
		.sync_e2ee(Some((pos, NEWS_POLL_TIMEOUT)))
		.await?;

	let waited = started.elapsed();

	news["rooms"][room.as_str()]
		.is_object()
		.into_option()
		.ok_or_else(|| err!("the round with news did not carry the new room"))?;

	waited
		.lt(&NEWS_DEADLINE)
		.into_option()
		.ok_or_else(|| err!("a round with news waited for the poll ({waited:?})"))?;

	Ok(())
}

/// One sliding sync carrying the e2ee extension.
///
/// Without a resume position it opens the connection and answers at once;
/// with one it long-polls for the given number of milliseconds.
#[implement(Client, params = "<'_>")]
async fn sync_e2ee(&self, resume: Option<(&str, u64)>) -> Result<Value> {
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

	let query =
		resume.map_or_else(String::new, |(pos, timeout)| format!("?pos={pos}&timeout={timeout}"));

	let url = format!(
		"{}/_matrix/client/unstable/org.matrix.simplified_msc3575/sync{query}",
		self.base
	);

	let response = self
		.post_url(&url, &body)
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(response)
}
