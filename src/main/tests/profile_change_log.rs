#![cfg(test)]

use std::{env::var, fs::remove_dir_all, path::PathBuf, process::id as process_id};

use futures::StreamExt;
use serde_json::json;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err,
	ruma::{UserId, profile::ProfileFieldName, user_id},
};
use tuwunel_service::Services;

const STATUS: &str = "org.matrix.msc4426.status";

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn change_log_bounds_its_range() -> Result {
	let root = var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
	let path = PathBuf::from(root).join(format!("tuwunel-profile-change-log-{}", process_id()));
	let db_path = DatabasePath(path);

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

	let stranger = user_id!("@statusbounds-stranger:localhost");

	expect_fields(services, stranger, (before, latest), &[], "another user's prefix").await
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
