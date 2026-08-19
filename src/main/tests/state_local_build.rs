#![allow(clippy::expect_used)]
#![allow(clippy::tests_outside_test_module)]
#![allow(clippy::unnecessary_debug_formatting)]

use std::{
	env::var, fs::remove_dir_all, iter::once, net::TcpListener, path::PathBuf,
	process::id as process_id, time::Duration,
};

use futures::{StreamExt, future::join};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err,
	matrix::{PduEvent, pdu::into_outgoing_federation},
	pdu::PduBuilder,
	ruma::{
		CanonicalJsonObject, EventId, OwnedRoomId, RoomId, UserId,
		events::room::message::RoomMessageEventContent,
	},
};
use tuwunel_database::Deserialized;
use tuwunel_service::{Services, rooms::short::ShortStateHash, users::Register};

#[test]
fn state_local_build_paths() -> Result {
	run_case(false)?;
	run_case(true)
}

fn run_case(resolve_state_locally: bool) -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let mode = if resolve_state_locally { "enabled" } else { "disabled" };
	let db_path =
		PathBuf::from(root).join(format!("tuwunel-state-local-build-{mode}-{}", process_id()));

	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.option.extend([
		format!("database_path={db_path:?}"),
		"address=[\"127.0.0.1\"]".to_owned(),
		format!("port={port}"),
		"listening=true".to_owned(),
		"log_enable=false".to_owned(),
		format!("resolve_state_locally={resolve_state_locally}"),
		"resolve_state_locally_shadow=false".to_owned(),
	]);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base, resolve_state_locally).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run_result, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	drop(server);
	drop(runtime);
	remove_dir_all(&db_path).ok();

	result
}

async fn exercise(services: &Services, base: &str, resolve_state_locally: bool) -> Result {
	wait_until_ready(services, base).await?;

	let user_id = UserId::parse_with_server_name("localbuild", services.globals.server_name())?;
	let token = "state-local-build-access-token-0001";

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("state-local-build-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	let room_id = create_room(services, base, token).await?;

	if resolve_state_locally {
		held_multi_prev_fork_resolves_locally(services, &user_id, &room_id).await
	} else {
		disabled_local_build_ignores_planted_memo(services, &user_id, &room_id).await
	}
}

async fn disabled_local_build_ignores_planted_memo(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let (held, held_json) = sign_message(services, user_id, room_id, "held").await?;

	services
		.timeline
		.add_pdu_outlier(&held.event_id, &held_json);

	suppress_upgrade(services, &held.event_id)?;

	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_forward_extremities(room_id, once(held.event_id.as_ref()), &state_lock)
		.await;

	drop(state_lock);

	let (incoming, incoming_json) = sign_message(services, user_id, room_id, "incoming").await?;
	let shortstatehash = services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	let resolved_state = services.db.get("eventid_resolvedstate")?;
	let incoming_event_id: &EventId = incoming.event_id.as_ref();

	resolved_state.raw_aput::<{ size_of::<ShortStateHash>() }, _, _>(
		incoming_event_id.as_bytes(),
		shortstatehash,
	);

	let planted: ShortStateHash = resolved_state
		.get(incoming_event_id)
		.await
		.deserialized()?;

	assert_eq!(planted, shortstatehash, "planted resolved-state memo did not round-trip");

	let room_version = services.state.get_room_version(room_id).await?;
	let incoming_json = into_outgoing_federation(incoming_json, &room_version);
	let result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			incoming_event_id,
			incoming_json,
			true,
		)
		.await;

	let Err(error) = result else {
		return Err!("disabled local build served the planted memo");
	};

	if !error
		.to_string()
		.contains("no candidate servers available")
	{
		return Err!("disabled local build failed before federation fallback: {error}");
	}

	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(incoming_event_id)
			.await
			.is_err_and(|error| error.is_not_found()),
		"incoming event unexpectedly reached the timeline"
	);
	assert!(
		services
			.timeline
			.pdu_exists(incoming_event_id)
			.await,
		"incoming event was not retained as an outlier"
	);

	Ok(())
}

async fn held_multi_prev_fork_resolves_locally(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let (left, left_json) = sign_message(services, user_id, room_id, "left").await?;
	let (right, right_json) = sign_message(services, user_id, room_id, "right").await?;

	services
		.timeline
		.add_pdu_outlier(&left.event_id, &left_json);

	services
		.timeline
		.add_pdu_outlier(&right.event_id, &right_json);

	suppress_upgrade(services, &left.event_id)?;
	suppress_upgrade(services, &right.event_id)?;

	let state_lock = services.state.mutex.lock(room_id).await;
	let prevs = [left.event_id.as_ref(), right.event_id.as_ref()];

	services
		.state
		.set_forward_extremities(room_id, prevs.into_iter(), &state_lock)
		.await;

	drop(state_lock);

	let (top, top_json) = sign_message(services, user_id, room_id, "top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	let shortstatehash = services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	let expected_state_len = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.count()
		.await;

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	assert_eq!(report.visited, 2, "local traversal missed a held parent");
	assert_eq!(report.forks, 1, "local traversal missed the fork");
	assert_eq!(report.memo_hits, 0, "local traversal used a memo");
	assert_eq!(report.gate_drops, 0, "local traversal dropped an event");
	assert_eq!(report.fallback, None, "local traversal used federation");
	assert_eq!(
		report.state_len,
		Some(expected_state_len),
		"local resolution changed the state size"
	);

	let room_version = services.state.get_room_version(room_id).await?;
	let top_json = into_outgoing_federation(top_json, &room_version);

	services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			top.event_id.as_ref(),
			top_json,
			true,
		)
		.await?;

	services
		.timeline
		.non_outlier_pdu_exists(top.event_id.as_ref())
		.await?;

	for parent in [left.event_id.as_ref(), right.event_id.as_ref()] {
		assert!(
			services
				.timeline
				.non_outlier_pdu_exists(parent)
				.await
				.is_err_and(|error| error.is_not_found()),
			"held parent unexpectedly reached the timeline"
		);
		assert!(
			services.timeline.pdu_exists(parent).await,
			"held parent disappeared from the outlier store"
		);
	}

	Ok(())
}

async fn sign_message(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	body: &str,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let state_lock = services.state.mutex.lock(room_id).await;
	let builder = PduBuilder::timeline(&RoomMessageEventContent::text_plain(body));

	services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await
}

fn suppress_upgrade(services: &Services, event_id: &EventId) -> Result {
	services
		.db
		.get("eventid_backoff")?
		.put((2_u8, event_id, 0_u32), (2_u64, u64::MAX));

	Ok(())
}

async fn wait_until_ready(services: &Services, base: &str) -> Result {
	let url = format!("{base}/_matrix/client/versions");

	timeout(Duration::from_secs(10), async {
		loop {
			if services
				.client
				.clients
				.default
				.get(&url)
				.send()
				.await
				.is_ok()
			{
				break;
			}

			sleep(Duration::from_millis(20)).await;
		}
	})
	.await
	.map_err(|_| err!("server listener did not become ready"))?;

	Ok(())
}

async fn create_room(services: &Services, base: &str, token: &str) -> Result<OwnedRoomId> {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/createRoom"))
		.bearer_auth(token)
		.json(&json!({}))
		.send()
		.await?
		.error_for_status()?
		.json::<Value>()
		.await?;

	let room_id = response
		.get("room_id")
		.and_then(Value::as_str)
		.ok_or_else(|| err!("createRoom response omitted room_id"))?;

	Ok(room_id.try_into()?)
}
