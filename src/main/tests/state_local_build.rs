#![allow(clippy::expect_used)]
#![allow(clippy::tests_outside_test_module)]
#![allow(clippy::unnecessary_debug_formatting)]

use std::{
	collections::BTreeSet, env::var, fs::remove_dir_all, iter::once, net::TcpListener,
	path::PathBuf, process::id as process_id, sync::Arc, time::Duration,
};

use futures::{
	StreamExt,
	future::{join, ready},
};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Error, Result, async_noinline, err,
	matrix::{PduEvent, pdu::into_outgoing_federation},
	pdu::PduBuilder,
	ruma::{
		CanonicalJsonObject, EventId, OwnedEventId, OwnedRoomId, RoomId, UserId,
		events::{
			StateEventType,
			room::{
				member::{MembershipState, RoomMemberEventContent},
				message::RoomMessageEventContent,
				name::RoomNameEventContent,
			},
		},
	},
};
use tuwunel_database::Deserialized;
use tuwunel_service::{
	Services,
	rooms::{short::ShortStateHash, state_compressor::CompressedState},
	users::Register,
};

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

#[async_noinline]
async fn exercise<'a>(
	services: &'a Services,
	base: &'a str,
	resolve_state_locally: bool,
) -> Result {
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

	if resolve_state_locally {
		let fork_room = create_room(services, base, token).await?;

		held_multi_prev_fork_resolves_locally(services, &user_id, &fork_room).await?;

		let denial_room = create_room(services, base, token).await?;

		positional_rejection_stays_uncommitted(services, &user_id, &denial_room).await?;

		let missing_create_room = create_room(services, base, token).await?;

		missing_create_falls_through_to_fetch(services, &user_id, &missing_create_room).await?;

		let soft_fail_room = create_room(services, base, token).await?;

		soft_failed_event_keeps_state_row(services, &user_id, &soft_fail_room).await
	} else {
		let room_id = create_room(services, base, token).await?;

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

async fn positional_rejection_stays_uncommitted(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let base = append_message(services, user_id, room_id, "position base").await?;
	let (denied, denied_json) = sign_state(services, user_id, room_id, "denied").await?;

	replace_state_before_without(
		services,
		room_id,
		&base,
		&StateEventType::RoomMember,
		user_id.as_str(),
	)
	.await?;

	let room_version = services.state.get_room_version(room_id).await?;
	let denied_json = into_outgoing_federation(denied_json, &room_version);
	let result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			denied.event_id.as_ref(),
			denied_json,
			true,
		)
		.await;

	assert!(
		matches!(&result, Err(Error::AuthCheck(..))),
		"positionally invalid event had an unexpected result: {result:?}"
	);

	assert!(
		services
			.timeline
			.pdu_exists(denied.event_id.as_ref())
			.await,
		"positionally rejected event was not retained as an outlier"
	);

	assert!(
		services
			.state
			.pdu_shortstatehash(denied.event_id.as_ref())
			.await
			.is_err_and(|error| error.is_not_found()),
		"positionally rejected event gained a state row"
	);

	suppress_upgrade(services, denied.event_id.as_ref())?;

	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_forward_extremities(room_id, once(denied.event_id.as_ref()), &state_lock)
		.await;

	drop(state_lock);

	let (top, top_json) = sign_message(services, user_id, room_id, "denial top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	assert_eq!(report.visited, 1, "local traversal missed the denied event");
	assert_eq!(report.gate_drops, 1, "gate denial was not counted exactly once");
	assert_eq!(report.fallback, None, "clean gate denial triggered a fetch");
	assert!(report.state_len.is_some(), "clean gate denial lost the built state");

	Ok(())
}

async fn append_message(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	body: &str,
) -> Result<OwnedEventId> {
	let builder = PduBuilder::timeline(&RoomMessageEventContent::text_plain(body));
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(builder, user_id, room_id, &state_lock)
		.await
}

async fn sign_state(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	name: &str,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let content = RoomNameEventContent::new(name.to_owned());
	let builder = PduBuilder::state(String::new(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await
}

async fn replace_state_before_without(
	services: &Services,
	room_id: &RoomId,
	event_id: &EventId,
	event_type: &StateEventType,
	state_key: &str,
) -> Result {
	let shortstatehash = services
		.state
		.pdu_shortstatehash(event_id)
		.await?;

	let shortstatekey = services
		.short
		.get_shortstatekey(event_type, state_key)
		.await?;

	let (state, excluded) = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.fold((Vec::new(), false), |(mut state, mut excluded), entry| {
			if entry.0 == shortstatekey {
				excluded = true;
			} else {
				state.push(entry);
			}

			ready((state, excluded))
		})
		.await;

	if !excluded {
		return Err!("state-before fixture lacks the selected key");
	}

	let compressed: CompressedState = services
		.state_compressor
		.compress_state_events(
			state
				.iter()
				.map(|(shortstatekey, event_id)| (shortstatekey, event_id.as_ref())),
		)
		.collect()
		.await;

	let compressed = Arc::new(compressed);

	services
		.state
		.set_event_state(event_id, room_id, compressed)
		.await?;

	Ok(())
}

async fn missing_create_falls_through_to_fetch(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let base = append_message(services, user_id, room_id, "missing create base").await?;
	let (held, held_json) = sign_state(services, user_id, room_id, "missing create").await?;

	replace_state_before_without(services, room_id, &base, &StateEventType::RoomCreate, "")
		.await?;

	services
		.timeline
		.add_pdu_outlier(&held.event_id, &held_json);

	suppress_upgrade(services, held.event_id.as_ref())?;

	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_forward_extremities(room_id, once(held.event_id.as_ref()), &state_lock)
		.await;

	drop(state_lock);

	let (top, top_json) = sign_message(services, user_id, room_id, "missing create top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	assert_eq!(report.gate_drops, 0, "missing create was counted as a denial");
	assert_eq!(
		report.fallback.as_deref(),
		Some("unevaluable"),
		"missing create used the wrong fallback"
	);

	assert_eq!(report.state_len, None, "missing create produced a state");

	let room_version = services.state.get_room_version(room_id).await?;
	let top_json = into_outgoing_federation(top_json, &room_version);
	let result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			top.event_id.as_ref(),
			top_json,
			true,
		)
		.await;

	let Err(error) = result else {
		return Err!("missing create did not fall through to federation fetch");
	};

	assert!(
		error
			.to_string()
			.contains("no candidate servers available"),
		"missing create failed before federation fetch: {error}"
	);

	Ok(())
}

#[async_noinline]
async fn soft_failed_event_keeps_state_row<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let (first, first_json) = sign_leave(services, user_id, room_id, "first leave").await?;
	let (delayed, delayed_json) = sign_leave(services, user_id, room_id, "delayed leave").await?;
	let mut original_prevs = first.prev_events.iter();
	let original_prev = original_prevs
		.next()
		.ok_or_else(|| err!("first leave has no predecessor"))?
		.to_owned();

	if original_prevs.next().is_some() {
		return Err!("first leave has multiple predecessors");
	}

	let top_event_id = prepare_soft_fail_descendant(
		services,
		user_id,
		room_id,
		&delayed,
		&delayed_json,
		&original_prev,
	)
	.await?;

	let room_version = services.state.get_room_version(room_id).await?;
	let first_json = into_outgoing_federation(first_json, &room_version);
	let first_result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			first.event_id.as_ref(),
			first_json,
			true,
		)
		.await?;

	assert!(first_result.is_some(), "first leave was not accepted");

	let delayed_json = into_outgoing_federation(delayed_json, &room_version);
	let delayed_result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			delayed.event_id.as_ref(),
			delayed_json,
			true,
		)
		.await?;

	assert_eq!(delayed_result, None, "delayed leave was not soft failed");
	assert!(
		services
			.pdu_metadata
			.is_event_soft_failed(delayed.event_id.as_ref())
			.await,
		"delayed leave lacks its soft-fail marker"
	);

	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(delayed.event_id.as_ref())
			.await
			.is_err_and(|error| error.is_not_found()),
		"soft-failed event reached the timeline"
	);

	assert!(
		services
			.timeline
			.pdu_exists(delayed.event_id.as_ref())
			.await,
		"soft-failed event disappeared from the outlier store"
	);

	let shortstatehash = services
		.state
		.pdu_shortstatehash(delayed.event_id.as_ref())
		.await?;

	let state_keys = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.map(|(shortstatekey, _)| shortstatekey)
		.collect::<BTreeSet<_>>()
		.await;

	let create = services
		.short
		.get_shortstatekey(&StateEventType::RoomCreate, "")
		.await?;

	let membership = services
		.short
		.get_shortstatekey(&StateEventType::RoomMember, user_id.as_str())
		.await?;

	assert!(state_keys.contains(&create), "soft-fail state row has no create event");
	assert!(
		state_keys.contains(&membership),
		"soft-fail state row has no positional membership"
	);

	let report = services
		.event_handler
		.local_state_report(&top_event_id)
		.await?;

	assert_eq!(report.visited, 1, "descendant walk missed its held predecessor");
	assert_eq!(report.gate_drops, 1, "descendant walk did not fold the soft-failed predecessor");

	assert_eq!(report.fallback, None, "descendant walk fell back");
	assert!(report.state_len.is_some(), "descendant walk produced no state");

	Ok(())
}

#[async_noinline]
async fn prepare_soft_fail_descendant<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
	delayed: &'a PduEvent,
	delayed_json: &'a CanonicalJsonObject,
	original_prev: &'a EventId,
) -> Result<OwnedEventId> {
	services
		.timeline
		.add_pdu_outlier(&delayed.event_id, delayed_json);

	set_forward_extremity(services, room_id, delayed.event_id.as_ref()).await;

	let (held, held_json) =
		Box::pin(sign_state(services, user_id, room_id, "held after leave")).await?;

	services
		.timeline
		.add_pdu_outlier(&held.event_id, &held_json);

	set_forward_extremity(services, room_id, held.event_id.as_ref()).await;

	let (top, top_json) =
		Box::pin(sign_message(services, user_id, room_id, "top after leave")).await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	set_forward_extremity(services, room_id, original_prev).await;

	Ok(top.event_id)
}

async fn set_forward_extremity(services: &Services, room_id: &RoomId, event_id: &EventId) {
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_forward_extremities(room_id, once(event_id), &state_lock)
		.await;
}

async fn sign_leave(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	reason: &str,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let content = RoomMemberEventContent {
		reason: Some(reason.to_owned()),
		..RoomMemberEventContent::new(MembershipState::Leave)
	};

	let builder = PduBuilder::state(user_id.to_string(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await
}

async fn sign_message(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	body: &str,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let builder = PduBuilder::timeline(&RoomMessageEventContent::text_plain(body));
	let state_lock = services.state.mutex.lock(room_id).await;

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
