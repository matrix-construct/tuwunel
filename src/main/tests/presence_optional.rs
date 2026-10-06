#![cfg(test)]

use std::fs::remove_dir_all;

use futures::StreamExt;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err,
	ruma::{UserId, presence::PresenceState},
};
use tuwunel_service::Services;

#[test]
fn optional_presence_preserves_data_errors() -> Result {
	let path = Args::test_database_path("presence-optional");

	let escaped = path
		.to_string_lossy()
		.replace('\\', "\\\\")
		.replace('"', "\\\"");

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option(format!("database_path=\"{escaped}\""));

	let args = Args { maintenance: true, ..args };
	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = exercise(&services).await;

		server.server.shutdown()?;
		drop(services);

		async_run(&server).await?;
		async_stop(&server).await?;

		outcome
	});

	drop(runtime);
	remove_dir_all(&path).ok();

	result
}

#[tracing::instrument(level = "trace", skip(services))]
async fn exercise(services: &Services) -> Result {
	let user_id =
		UserId::parse_with_server_name("presence-reader", services.globals.server_name())?;

	let index = &services.db["userid_presenceid"];
	let payload = &services.db["presenceid_presence"];
	let count = 1_u64.to_be_bytes();
	let key = [count.as_slice(), user_id.as_bytes()].concat();
	let absent = services
		.presence
		.get_presence_optional(&user_id)
		.await?;

	absent
		.is_none()
		.then_some(())
		.ok_or_else(|| err!("missing presence index did not return None"))?;

	index.insert(user_id.as_bytes(), count);

	services
		.presence
		.get_presence_optional(&user_id)
		.await
		.is_err()
		.then_some(())
		.ok_or_else(|| err!("dangling presence index did not return an error"))?;

	let stored = br#"{"state":"unavailable","currently_active":false,"last_active_ts":0,"status_msg":"away"}"#;

	payload.insert(&key, stored);

	let event = services
		.presence
		.get_presence_optional(&user_id)
		.await?
		.ok_or_else(|| err!("stored presence did not return Some"))?;

	(event.sender == user_id
		&& event.content.presence == PresenceState::Unavailable
		&& event.content.currently_active == Some(false)
		&& event.content.status_msg.as_deref() == Some("away"))
	.then_some(())
	.ok_or_else(|| err!("stored presence fields changed"))?;

	payload.insert(&key, b"{");

	services
		.presence
		.get_presence_optional(&user_id)
		.await
		.is_err()
		.then_some(())
		.ok_or_else(|| err!("corrupt presence payload did not return an error"))?;

	payload.insert(&key, stored);
	index.insert(user_id.as_bytes(), b"\x01");

	services
		.presence
		.get_presence_optional(&user_id)
		.await
		.is_err()
		.then_some(())
		.ok_or_else(|| err!("corrupt presence index did not return an error"))?;

	window_bounds(services, &user_id, stored).await;
	wire_update(services, &user_id)
}

async fn window_bounds(services: &Services, user_id: &UserId, stored: &[u8]) {
	let payload = &services.db["presenceid_presence"];
	let base = 1_u64 << 63;
	let counts = [base + 254, base + 255, base + 256, base + 257, u64::MAX];

	for count in counts {
		let key = [count.to_be_bytes().as_slice(), user_id.as_bytes()].concat();

		payload.insert(&key, stored);
	}

	let invalid = [(base + 256).to_be_bytes().as_slice(), b"invalid"].concat();

	payload.insert(&invalid, stored);

	let cases: &[(u64, Option<u64>, &[u64])] = &[
		(base + 254, Some(base + 256), &counts[1..3]),
		(base + 255, Some(base + 256), &counts[2..3]),
		(base + 255, None, &counts[2..]),
		(base + 256, Some(base + 256), &[]),
		(base + 257, Some(base + 255), &[]),
		(u64::MAX - 1, Some(u64::MAX), &counts[4..]),
		(u64::MAX, None, &[]),
	];

	for &(since, to, expected) in cases {
		let rows: Vec<_> = services
			.presence
			.presence_since(since, to)
			.map(|(user_id, count, bytes)| (user_id.to_owned(), count, bytes.to_owned()))
			.collect()
			.await;

		assert!(
			rows.iter()
				.map(|(_, count, _)| count)
				.eq(expected)
		);

		assert!(
			rows.iter()
				.all(|(user, _, bytes)| user == user_id && bytes == stored)
		);
	}
}

fn wire_update(services: &Services, user_id: &UserId) -> Result {
	let stored = br#"{"state":"unavailable","currently_active":false,"last_active_ts":18446744073709551615,"status_msg":"away"}"#;
	let update = services
		.presence
		.from_json_bytes_to_update(stored, user_id)?;

	assert_eq!(update.user_id, user_id);
	assert_eq!(update.presence, PresenceState::Unavailable);
	assert!(!update.currently_active);
	assert_eq!(update.status_msg.as_deref(), Some("away"));
	assert_eq!(u64::from(update.last_active_ago), 0);
	services
		.presence
		.from_json_bytes_to_update(b"{", user_id)
		.expect_err("invalid presence JSON must fail");

	Ok(())
}
