#![cfg(test)]

#[path = "signature_updates/mod.rs"]
mod fixture;

use std::{env::temp_dir, process::id};

use futures::{FutureExt, StreamExt};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	ruma::{UserId, device_id, room_id},
};
use tuwunel_database::{Interfix, Json};
use tuwunel_service::Services;

use self::fixture::{Fixture, assert_changes, register_appservice};

#[test]
fn signatures_are_canonical_and_foreign_updates_are_private() -> Result {
	let path = temp_dir()
		.join("tuwunel")
		.join(format!("signature-updates-{}", id()));

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option(format!("database_path={path:?}"))
		.with_option("allow_local_presence=false")
		.with_option("allow_outgoing_presence=false")
		.with_option("device_key_update_encrypted_rooms_only=false");

	let args = Args { maintenance: true, ..args };

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = exercise(&services).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	})
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services) -> Result {
	let sender = UserId::parse_with_server_name("signer", services.globals.server_name())?;
	let target = UserId::parse_with_server_name("target", services.globals.server_name())?;
	let room = room_id!("!signature-updates:localhost");
	let fixture = Fixture::new(&sender, &target)?;

	fixture.store(services)?;

	services.db["userroomid_joined"].put((&target, room), 1_u64);
	register_appservice(services).await?;

	let before = services.globals.current_count();

	fixture
		.upload(services, &fixture.signature)
		.await?;

	let first = services.globals.current_count();

	let written = services.db["keyid_key"].watch_prefix((&target, &fixture.target_key));

	for spelling in fixture.spellings() {
		fixture.upload(services, &spelling).await?;
	}

	assert_eq!(services.globals.current_count(), first, "equivalent encodings");
	assert_eq!(first.checked_sub(before), Some(1), "private update allocates one count");
	assert!(written.now_or_never().is_none(), "equivalent encodings rewrote row");

	assert_changes(services, &sender, before, &[&target]).await?;
	assert_changes(services, &target, before, &[]).await?;
	assert!(
		services
			.users
			.room_keys_changed(room, before, None)
			.count()
			.await == 0
	);

	let other = Fixture::new(&sender, &target)?.with_target(&fixture)?;

	other.store_signer(services)?;
	let watcher = services.db["keychangeid_userid"].watch_prefix((&target, Interfix));
	let room_watcher = services.db["keychangeid_userid"].watch_prefix((room, Interfix));
	let signer_watcher = services.db["keychangeid_userid"].watch_prefix((&sender, Interfix));
	let appservice_watcher = services.db["servernameevent_data"].watch_raw_prefix(b"+");
	let padded = format!("{}==", other.signature);

	other.upload(services, &padded).await?;
	let changed = services.globals.current_count();

	assert_eq!(changed.checked_sub(first), Some(1), "real signature change");
	assert!(signer_watcher.now_or_never().is_some());
	assert!(watcher.now_or_never().is_none());
	assert!(room_watcher.now_or_never().is_none());
	assert!(appservice_watcher.now_or_never().is_none());
	assert_changes(services, &sender, first, &[&target]).await?;
	assert_eq!(other.stored(services).await?, other.signed_key(&other.signature));

	let legacy = other.signed_key(&padded);

	services.db["keyid_key"].put((&target, &other.target_key), Json(legacy));
	let before = services.globals.current_count();

	other.upload(services, &other.signature).await?;
	let normalized = services.globals.current_count();

	assert_eq!(normalized.checked_sub(before), Some(1), "legacy normalization");
	assert_changes(services, &sender, before, &[&target]).await?;
	assert_changes(services, &target, before, &[]).await?;
	assert_eq!(other.stored(services).await?, other.signed_key(&other.signature));

	for spelling in other.spellings() {
		other.upload(services, &spelling).await?;
		assert_eq!(services.globals.current_count(), normalized);
	}

	let own = Fixture::new(&target, &target)?;

	own.store_device_signer(services, device_id!("SIGNER"))?;
	let before = services.globals.current_count();
	let appservice_watcher = services.db["servernameevent_data"].watch_raw_prefix(b"+");
	let padded = format!("{}==", own.signature);

	own.upload(services, &padded).await?;
	assert_eq!(own.stored(services).await?, own.signed_key(&own.signature));
	assert_changes(services, &target, before, &[&target]).await?;
	assert_eq!(
		services
			.users
			.room_keys_changed(room, before, None)
			.count()
			.await,
		1
	);

	assert!(appservice_watcher.now_or_never().is_some());

	let after = services.globals.current_count();

	for spelling in own.spellings() {
		own.upload(services, &spelling).await?;
		assert_eq!(services.globals.current_count(), after, "same-user equivalence");
	}

	let legacy = own.signed_key(&padded);

	services.db["keyid_key"].put((&target, &own.target_key), Json(legacy));
	own.upload(services, &own.signature).await?;
	assert_changes(services, &target, after, &[&target]).await?;
	assert_eq!(own.stored(services).await?, own.signed_key(&own.signature));

	let normalized = services.globals.current_count();
	let written = services.db["keyid_key"].watch_prefix((&target, &own.target_key));

	for spelling in own.spellings() {
		own.upload(services, &spelling).await?;
	}

	assert_eq!(services.globals.current_count(), normalized);
	assert!(written.now_or_never().is_none());

	Ok(())
}
