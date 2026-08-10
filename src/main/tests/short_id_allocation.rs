#![cfg(test)]

use std::{env::var, fs::remove_dir_all, path::PathBuf, process::id as process_id};

use futures::{StreamExt, pin_mut};
use ruma::events::room::create::RoomCreateEventContent;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	ruma::{OwnedEventId, RoomVersionId, event_id, room_id},
	utils::stream::ReadyExt,
};
use tuwunel_service::{Services, pdu::PduBuilder};

const OCCURRENCES: usize = 8;

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn batch_duplicates_share_one_shorteventid() -> Result {
	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path = DatabasePath(
		PathBuf::from(root).join(format!("tuwunel-short-id-allocation-{}", process_id())),
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
	create_hash_and_sign_does_not_allocate_short_id(services).await?;

	let event_id = event_id!("$short-id-allocation-batch:localhost");
	// a repeated event misses the batched lookup on every occurrence
	let batch = [event_id; OCCURRENCES];

	let shorts = services
		.short
		.multi_get_or_create_shorteventid(batch.iter().copied());

	pin_mut!(shorts);

	let Some(first) = shorts.next().await else {
		return Err!("batch yielded no short ids");
	};

	if shorts.ready_any(|short| short.ne(&first)).await {
		return Err!("one event id took more than one short id within a batch");
	}

	let resolved: OwnedEventId = services
		.short
		.get_eventid_from_short(first)
		.await?;

	if resolved != event_id {
		return Err!("short id did not resolve back to its event id");
	}

	Ok(())
}

async fn create_hash_and_sign_does_not_allocate_short_id(services: &Services) -> Result {
	let sender = services.globals.server_user.as_ref();
	if !services.users.exists(sender).await {
		services.users.create(sender, None, None).await?;
	}

	let room_id = room_id!("!short-id-no-append:localhost");
	let state_lock = services.state.mutex.lock(room_id).await;
	let (pdu, pdu_json, _prev_state) = services
		.timeline
		.create_hash_and_sign_event(
			PduBuilder::state(String::new(), &RoomCreateEventContent {
				federate: true,
				predecessor: None,
				room_version: RoomVersionId::V11,
				..RoomCreateEventContent::new_v11()
			}),
			sender,
			room_id,
			&state_lock,
		)
		.await?;
	let event_id = pdu.event_id.clone();

	if services
		.short
		.get_shorteventid(&event_id)
		.await
		.is_ok()
	{
		return Err!("create_hash_and_sign_event allocated a short event id before append");
	}

	services
		.timeline
		.append_created_pdu(pdu, pdu_json, sender, &state_lock)
		.await?;

	if services
		.short
		.get_shorteventid(&event_id)
		.await
		.is_err()
	{
		return Err!("append_created_pdu did not allocate a short event id");
	}

	Ok(())
}
