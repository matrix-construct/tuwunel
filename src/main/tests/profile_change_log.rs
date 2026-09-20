#![cfg(test)]

use std::{fs::remove_dir_all, path::PathBuf};

use futures::StreamExt;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err,
	ruma::{UserId, profile::ProfileFieldName, user_id},
	utils::{BoolExt, TryReadyExt, result::NotFound},
};
use tuwunel_service::Services;

const STATUS: &str = "org.matrix.msc4426.status";
const CALL: &str = "org.matrix.msc4426.call";

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn change_log_bounds_its_range() -> Result {
	let db_path = DatabasePath(Args::test_database_path("profile-change-log"));

	let mut args = Args {
		maintenance: true,
		..Args::default_test(&["fresh", "cleanup"])
	};

	args.option
		.push(format!("database_path={:?}", db_path.0));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = assert_change_log_bounds(&services).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}

async fn assert_change_log_bounds(services: &Services) -> Result {
	let user_id = user_id!("@statusbounds:localhost");
	let before = services.globals.current_count();

	set_status(services, user_id, "away").await?;

	let after = services.globals.current_count();

	// The low bound is exclusive so replaying a delivered token delivers
	// nothing again, and the high bound is inclusive.
	expect_fields(services, user_id, (before, after), &[STATUS], "the write").await?;
	expect_fields(services, user_id, (after, after), &[], "the write at the low bound").await?;
	expect_fields(services, user_id, (before, before), &[], "the write past the high bound")
		.await?;

	set_status(services, user_id, "back").await?;

	let latest = services.globals.current_count();

	expect_fields(services, user_id, (after, latest), &[STATUS], "the second write").await?;

	set_status(services, user_id, "back").await?;

	let restated = services.globals.current_count();

	// Saving a value again is how a user repairs a client whose copy went stale.
	expect_fields(services, user_id, (latest, restated), &[STATUS], "the restated write").await?;

	set_call(services, user_id).await?;

	let before_clear = services.globals.current_count();

	expect_fields(services, user_id, (restated, before_clear), &[CALL], "the call write").await?;

	services
		.profile
		.clear_profile_keys(user_id)
		.await?;

	let cleared = services.globals.current_count();

	expect_fields(
		services,
		user_id,
		(before_clear, cleared),
		&[CALL, STATUS],
		"the profile clear",
	)
	.await?;

	for name in [CALL, STATUS] {
		let field = ProfileFieldName::from(name);
		let value: Result<Value> = services
			.profile
			.profile_key(user_id, &field)
			.await;

		assert!(value.is_not_found(), "{name} must be cleared");
	}

	let remote = user_id!("@statusbounds:remote.test");
	let before_fetch = services.globals.current_count();

	set_status(services, remote, "away").await?;

	let fetched = services.globals.current_count();

	expect_fields(services, remote, (before_fetch, fetched), &[STATUS], "a remote write").await?;

	// A remote profile refresh reissues every field on each lookup.
	set_status(services, remote, "away").await?;

	expect_fields(services, remote, (fetched, u64::MAX), &[], "a restated remote write").await?;

	BoolExt::ok_or_else(services.globals.current_count() == fetched, || {
		err!("a restated remote write took a count")
	})?;

	let stranger = user_id!("@statusbounds-stranger:localhost");

	expect_fields(services, stranger, (before, cleared), &[], "another user's prefix").await?;
	assert_corrupt_range_boundaries(services).await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn assert_corrupt_range_boundaries(services: &Services) -> Result {
	let user_id = user_id!("@zzprofilebounds:localhost");
	let next_user = user_id!("@zzprofileboundt:localhost");
	let changes = &services.db["profilechangeid_userid"];

	changes.put_raw((user_id, 10_u64, STATUS), user_id.as_bytes());
	changes.put_raw((user_id, 20_u64, STATUS), b"not-a-user-id");
	changes.put_raw((next_user, 10_u64, STATUS), b"not-a-user-id");

	expect_checked_fields(services, user_id, (0, 10), &[STATUS]).await?;
	assert!(
		services
			.profile
			.try_profile_changed(user_id, 10, Some(20))
			.ready_try_fold((), |(), _| Ok(()))
			.await
			.is_err(),
		"in-range corrupt values must fail discovery"
	);

	changes.del((user_id, 20_u64, STATUS));
	expect_checked_fields(services, user_id, (0, 20), &[STATUS]).await?;

	let invalid_field = (user_id, 11_u64, &b"\x80"[..]);

	changes.put_raw(invalid_field, user_id.as_bytes());
	expect_checked_fields(services, user_id, (0, 10), &[STATUS]).await?;
	assert!(
		services
			.profile
			.try_profile_changed(user_id, 10, Some(11))
			.ready_try_fold((), |(), _| Ok(()))
			.await
			.is_err(),
		"in-range corrupt keys must fail discovery"
	);

	expect_checked_fields(services, user_id, (10, 10), &[]).await?;
	expect_checked_fields(services, user_id, (20, 10), &[]).await?;
	changes.del(invalid_field);

	changes.put_raw((user_id, u64::MAX, STATUS), b"not-a-user-id");
	expect_checked_fields(services, user_id, (u64::MAX, u64::MAX), &[]).await?;
	assert!(
		services
			.profile
			.try_profile_changed(user_id, u64::MAX, None)
			.ready_try_fold((), |(), _| Ok(()))
			.await
			.is_ok(),
		"an exhausted unbounded range must not replay its last row"
	);

	changes.put_raw((user_id, u64::MAX, STATUS), user_id.as_bytes());
	expect_checked_fields(services, user_id, (u64::MAX - 1, u64::MAX), &[STATUS]).await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn expect_checked_fields(
	services: &Services,
	user_id: &UserId,
	(from, to): (u64, u64),
	expected: &[&str],
) -> Result {
	let count = services
		.profile
		.try_profile_changed(user_id, from, Some(to))
		.ready_try_fold(0_usize, |index, (_, field)| {
			assert_eq!(Some(field), expected.get(index).copied());

			Ok(index.saturating_add(1))
		})
		.await?;

	assert_eq!(count, expected.len());

	Ok(())
}

async fn set_status(services: &Services, user_id: &UserId, text: &str) -> Result {
	let status = json!({ "text": text, "emoji": "🌴" });

	services
		.profile
		.set_profile_keys(user_id, &[(ProfileFieldName::from(STATUS), Some(status))], None)
		.await
}

async fn expect_fields(
	services: &Services,
	user_id: &UserId,
	(from, to): (u64, u64),
	expected: &[&str],
	subject: &str,
) -> Result {
	let changed: Vec<String> = services
		.profile
		.profile_changed(user_id, from, Some(to))
		.map(|(_, field)| field.to_owned())
		.collect()
		.await;

	changed
		.iter()
		.map(String::as_str)
		.eq(expected.iter().copied())
		.then_some(())
		.ok_or_else(|| err!("{subject} reported {changed:?}, expected {expected:?}"))
}

async fn set_call(services: &Services, user_id: &UserId) -> Result {
	let call = json!({ "call_joined_ts": 1 });

	services
		.profile
		.set_profile_keys(user_id, &[(ProfileFieldName::from(CALL), Some(call))], None)
		.await
}
