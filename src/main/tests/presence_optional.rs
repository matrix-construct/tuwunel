#![cfg(test)]

use std::{env::temp_dir, fs::remove_dir_all, process::id as process_id};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err,
	ruma::{UserId, presence::PresenceState},
};
use tuwunel_service::Services;

#[test]
fn optional_presence_preserves_data_errors() -> Result {
	let path = temp_dir()
		.join("tuwunel")
		.join(format!("presence-optional-{}", process_id()));

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
		.ok_or_else(|| err!("corrupt presence index did not return an error"))
}
