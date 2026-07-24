#![cfg(test)]

use std::{fs::remove_dir_all, path::PathBuf, process::id as process_id, sync::Arc};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, PduEvent, Result,
	ruma::{
		CanonicalJsonObject, event_id, events::TimelineEventType, room_id, serde::Raw, uint,
		user_id,
	},
};
use tuwunel_service::{Services, rooms::state_compressor::CompressedState};

const SUCCESS_HASH: [u8; 32] = [0xA5; 32];
const FAILURE_HASH: [u8; 32] = [0x5A; 32];

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn state_hash_allocation_persists_an_atomic_pair() -> Result {
	let db_path = DatabasePath(
		PathBuf::from("/nvme/target/tmp")
			.join(format!("tuwunel-state-atomic-allocation-{}", process_id())),
	);
	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.maintenance = true;
	args.option
		.push(format!("database_path={:?}", db_path.0));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = exercise(&services).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}

async fn exercise(services: &Services) -> Result {
	let statediff = Arc::new(CompressedState::new());
	let (shortstatehash, already_existed) = services
		.short
		.get_or_create_shortstatehash(&SUCCESS_HASH, |txn, shortstatehash| {
			services.state_compressor.save_state_from_diff(
				txn,
				shortstatehash,
				statediff.clone(),
				statediff.clone(),
				1,
				Vec::new(),
			)
		})
		.await?;

	if already_existed {
		return Err!("new state hash reported as existing");
	}

	if services
		.short
		.get_shortstatehash(&SUCCESS_HASH)
		.await?
		!= shortstatehash
	{
		return Err!("state hash mapping did not resolve to its allocation");
	}

	let state = services
		.state_compressor
		.load_shortstatehash_info(shortstatehash)
		.await?;

	if state.len() != 1 || !state[0].full_state.is_empty() {
		return Err!("empty state diff did not load as one empty layer");
	}

	if !services.globals.pending_count().is_empty() {
		return Err!("successful allocation left a count permit pending");
	}

	let failure = services
		.short
		.get_or_create_shortstatehash(&FAILURE_HASH, |_, _| Err!("deliberate state diff failure"))
		.await;

	if failure.is_ok() {
		return Err!("failure callback did not abort allocation");
	}

	if services
		.short
		.get_shortstatehash(&FAILURE_HASH)
		.await
		.is_ok()
	{
		return Err!("failure callback published the state hash mapping");
	}

	if !services.globals.pending_count().is_empty() {
		return Err!("failure callback left a count permit pending");
	}

	let (existing, already_existed) = services
		.short
		.get_or_create_shortstatehash(&SUCCESS_HASH, |_, _| {
			Err!("existing state hash invoked the state diff callback")
		})
		.await?;

	if !already_existed || existing != shortstatehash {
		return Err!("existing state hash did not retain its allocation");
	}

	let appended = services
		.state
		.append_to_state(&state_pdu()?)
		.await?;

	let state = services
		.state_compressor
		.load_shortstatehash_info(appended)
		.await?;

	let Some(state) = state.last() else {
		return Err!("appended state had no diff layer");
	};

	if state.shortstatehash != appended || state.full_state.len() != 1 {
		return Err!("appended state event was not loaded");
	}

	Ok(())
}

fn state_pdu() -> Result<PduEvent> {
	Ok(PduEvent {
		kind: TimelineEventType::RoomCreate,
		content: Raw::new(&CanonicalJsonObject::new())?,
		event_id: event_id!("$atomic-state-allocation:localhost").to_owned(),
		room_id: room_id!("!atomic-state-allocation:localhost").to_owned(),
		sender: user_id!("@atomic-state-allocation:localhost").to_owned(),
		state_key: Some("".into()),
		redacts: None,
		prev_events: Default::default(),
		auth_events: Default::default(),
		origin_server_ts: uint!(0),
		depth: uint!(1),
		hashes: Default::default(),
		origin: None,
		unsigned: None,
	})
}
