#![allow(clippy::expect_used)]
#![allow(clippy::tests_outside_test_module)]
#![allow(clippy::unnecessary_debug_formatting)]

use std::{
	collections::BTreeSet,
	env::var,
	fs::remove_dir_all,
	iter::once,
	net::TcpListener,
	path::PathBuf,
	process::id as process_id,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use futures::{
	StreamExt, TryStreamExt,
	future::{BoxFuture, join, ready},
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
use tuwunel_database::{Deserialized, serialize_key};
use tuwunel_service::{
	Services,
	rooms::{
		event_handler::StateLocalMetrics, short::ShortStateHash,
		state_compressor::CompressedState,
	},
	users::Register,
};

#[derive(Clone, Copy)]
enum Case {
	Disabled,
	Baseline,
	MissingStateDiff,
	MissingEventReverse,
	MissingStateKeyReverse,
	MissingNamedPdu,
	MissingAuthAncestor,
	MalformedAuthAncestor,
	CorruptChainCache,
	DirectMemoFailure,
	WalkMemoFailure,
	DegreeOneStateMiss,
	SiblingStateMiss,
	UnpolledChain,
	InteriorForkSentinel,
}

#[derive(Clone, Copy)]
enum AncestorFailure {
	Missing,
	Malformed,
	InteriorFork,
}

#[derive(Clone, Copy)]
enum PduFailure {
	Missing,
	Malformed,
}

const CASES: [Case; 15] = [
	Case::Disabled,
	Case::Baseline,
	Case::MissingStateDiff,
	Case::MissingEventReverse,
	Case::MissingStateKeyReverse,
	Case::MissingNamedPdu,
	Case::MissingAuthAncestor,
	Case::MalformedAuthAncestor,
	Case::CorruptChainCache,
	Case::DirectMemoFailure,
	Case::WalkMemoFailure,
	Case::DegreeOneStateMiss,
	Case::SiblingStateMiss,
	Case::UnpolledChain,
	Case::InteriorForkSentinel,
];

#[test]
fn state_local_build_paths() -> Result { CASES.into_iter().try_for_each(run_case) }

fn run_case(case: Case) -> Result {
	let name = case_name(case);
	let case_error = |error: Error| err!("state local build case {name} failed: {error}");
	let listener =
		TcpListener::bind(("127.0.0.1", 0)).map_err(|error| case_error(error.into()))?;
	let port = listener
		.local_addr()
		.map_err(|error| case_error(error.into()))?
		.port();

	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path =
		PathBuf::from(root).join(format!("tuwunel-state-local-build-{name}-{}", process_id()));

	let resolve_state_locally = !matches!(case, Case::Disabled);

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

	let runtime = Runtime::new(Some(&args)).map_err(&case_error)?;
	let server = Server::new(Some(&args), Some(&runtime)).map_err(&case_error)?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base, case).await;
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

	result.map_err(case_error)
}

fn case_name(case: Case) -> &'static str {
	match case {
		| Case::Disabled => "disabled",
		| Case::Baseline => "baseline",
		| Case::MissingStateDiff => "missing-state-diff",
		| Case::MissingEventReverse => "missing-event-reverse",
		| Case::MissingStateKeyReverse => "missing-state-key-reverse",
		| Case::MissingNamedPdu => "missing-named-pdu",
		| Case::MissingAuthAncestor => "missing-auth-ancestor",
		| Case::MalformedAuthAncestor => "malformed-auth-ancestor",
		| Case::CorruptChainCache => "corrupt-chain-cache",
		| Case::DirectMemoFailure => "direct-memo-failure",
		| Case::WalkMemoFailure => "walk-memo-failure",
		| Case::DegreeOneStateMiss => "degree-one-state-miss",
		| Case::SiblingStateMiss => "sibling-state-miss",
		| Case::UnpolledChain => "unpolled-chain",
		| Case::InteriorForkSentinel => "interior-fork-sentinel",
	}
}

#[async_noinline]
async fn exercise<'a>(services: &'a Services, base: &'a str, case: Case) -> Result {
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

	exercise_case(services, base, token, &user_id, case).await
}

fn exercise_case<'a>(
	services: &'a Services,
	base: &'a str,
	token: &'a str,
	user_id: &'a UserId,
	case: Case,
) -> BoxFuture<'a, Result> {
	match case {
		| Case::Baseline => Box::pin(enabled_baseline(services, base, token, user_id)),
		| Case::MissingStateDiff => Box::pin(missing_state_diff(services, base, token, user_id)),
		| Case::MissingEventReverse =>
			Box::pin(missing_event_reverse(services, base, token, user_id)),
		| Case::MissingStateKeyReverse =>
			Box::pin(missing_state_key_reverse(services, base, token, user_id)),
		| Case::MissingNamedPdu => Box::pin(missing_named_pdu(services, base, token, user_id)),
		| Case::MissingAuthAncestor => Box::pin(auth_ancestor_failure(
			services,
			base,
			token,
			user_id,
			AncestorFailure::Missing,
		)),
		| Case::MalformedAuthAncestor => Box::pin(auth_ancestor_failure(
			services,
			base,
			token,
			user_id,
			AncestorFailure::Malformed,
		)),
		| Case::CorruptChainCache =>
			Box::pin(corrupt_chain_cache_rebuilds(services, base, token, user_id)),
		| Case::DirectMemoFailure =>
			Box::pin(direct_memo_failure_is_miss(services, base, token, user_id)),
		| Case::WalkMemoFailure =>
			Box::pin(walk_memo_failure_is_unevaluable(services, base, token, user_id)),
		| Case::DegreeOneStateMiss =>
			Box::pin(degree_one_state_miss(services, base, token, user_id)),
		| Case::SiblingStateMiss => Box::pin(sibling_state_miss(services, base, token, user_id)),
		| Case::UnpolledChain =>
			Box::pin(unpolled_chain_stays_clear(services, base, token, user_id)),
		| Case::InteriorForkSentinel => Box::pin(auth_ancestor_failure(
			services,
			base,
			token,
			user_id,
			AncestorFailure::InteriorFork,
		)),
		| Case::Disabled => Box::pin(async move {
			let room_id = create_room(services, base, token).await?;

			disabled_local_build_ignores_planted_memo(services, user_id, &room_id).await
		}),
	}
}

async fn enabled_baseline(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let step_error = |step: &str, error: Error| err!("baseline {step} failed: {error}");

	let fork_room = create_room(services, base, token)
		.await
		.map_err(|error| step_error("held multi-prev fork", error))?;

	held_multi_prev_fork_resolves_locally(services, user_id, &fork_room)
		.await
		.map_err(|error| step_error("held multi-prev fork", error))?;

	let denial_room = create_room(services, base, token)
		.await
		.map_err(|error| step_error("positional rejection", error))?;

	positional_rejection_stays_uncommitted(services, user_id, &denial_room)
		.await
		.map_err(|error| step_error("positional rejection", error))?;

	let missing_create_room = create_room(services, base, token)
		.await
		.map_err(|error| step_error("missing-create fallback", error))?;

	missing_create_falls_through_to_fetch(services, user_id, &missing_create_room)
		.await
		.map_err(|error| step_error("missing-create fallback", error))?;

	let soft_fail_room = create_room(services, base, token)
		.await
		.map_err(|error| step_error("soft-failed state row", error))?;

	soft_failed_event_keeps_state_row(services, user_id, &soft_fail_room)
		.await
		.map_err(|error| step_error("soft-failed state row", error))
}

async fn missing_state_diff(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let anchor = append_message(services, user_id, &room_id, "state diff anchor").await?;
	let intact_state = services.state.pdu_shortstatehash(&anchor).await?;
	append_state(services, user_id, &room_id, "state diff change").await?;
	let boundary = append_message(services, user_id, &room_id, "state diff boundary").await?;
	let (held, top, top_json) =
		held_message_chain(services, user_id, &room_id, &boundary).await?;
	let corrupt_state = services
		.state
		.pdu_shortstatehash(&boundary)
		.await?;

	assert_ne!(intact_state, corrupt_state, "state diff fixture reused the intact state");
	restore_room_state(services, &room_id, intact_state, &anchor).await;
	remove_short_row(services, "shortstatehash_statediff", corrupt_state).await?;
	assert_eq!(
		services
			.state
			.get_room_shortstatehash(&room_id)
			.await?,
		intact_state,
		"state diff fixture did not restore the current room state",
	);
	services
		.state_accessor
		.state_full_ids_strict(intact_state)
		.try_collect::<Vec<_>>()
		.await
		.map_err(|error| err!("state diff fixture corrupted the restored state: {error}"))?;
	suppress_upgrade(services, held.event_id.as_ref())?;
	assert_unevaluable(services, top.event_id.as_ref(), "missing state diff").await?;
	assert_fetches(
		services,
		&room_id,
		&top,
		top_json,
		ExpectedWalkOutcome::Unevaluable,
		"missing state diff",
	)
	.await
}

async fn missing_event_reverse(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let named = append_state(services, user_id, &room_id, "reverse mapping state").await?;
	let boundary =
		append_message(services, user_id, &room_id, "reverse mapping boundary").await?;
	let (held, top, top_json) =
		held_message_chain(services, user_id, &room_id, &boundary).await?;
	let shorteventid = services.short.get_shorteventid(&named).await?;

	remove_short_row(services, "shorteventid_eventid", shorteventid).await?;
	suppress_upgrade(services, held.event_id.as_ref())?;
	assert_unevaluable(services, top.event_id.as_ref(), "missing event reverse map").await?;
	assert_fetches(
		services,
		&room_id,
		&top,
		top_json,
		ExpectedWalkOutcome::Unevaluable,
		"missing event reverse map",
	)
	.await
}

async fn missing_state_key_reverse(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;

	append_state(services, user_id, &room_id, "state key boundary").await?;

	let (left, right, top, top_json) = held_state_fork(services, user_id, &room_id).await?;
	let shortstatekey = services
		.short
		.get_shortstatekey(&StateEventType::RoomName, "")
		.await?;

	remove_short_row(services, "shortstatekey_statekey", shortstatekey).await?;
	suppress_upgrade(services, left.event_id.as_ref())?;
	suppress_upgrade(services, right.event_id.as_ref())?;
	assert_unevaluable(services, top.event_id.as_ref(), "missing state key reverse map").await?;
	assert_fetches(
		services,
		&room_id,
		&top,
		top_json,
		ExpectedWalkOutcome::Unevaluable,
		"missing state key reverse map",
	)
	.await
}

async fn missing_named_pdu(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let missing = append_state(services, user_id, &room_id, "named pdu left").await?;
	let left = append_message(services, user_id, &room_id, "named pdu left boundary").await?;

	append_state(services, user_id, &room_id, "named pdu right").await?;

	let right = append_message(services, user_id, &room_id, "named pdu right boundary").await?;

	set_forward_extremities(services, &room_id, [left.as_ref(), right.as_ref()]).await;

	let (fork, fork_json) = sign_message(services, user_id, &room_id, "named pdu fork").await?;

	services
		.timeline
		.add_pdu_outlier(&fork.event_id, &fork_json);

	set_forward_extremity(services, &room_id, fork.event_id.as_ref()).await;

	let (top, top_json) = sign_message(services, user_id, &room_id, "named pdu top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	corrupt_timeline_pdu(services, &missing, PduFailure::Missing).await?;
	suppress_upgrade(services, fork.event_id.as_ref())?;

	let before_report = services.event_handler.state_local_metrics();

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	let after_report = services.event_handler.state_local_metrics();

	assert_eq!(after_report, before_report, "local state diagnostic changed production metrics");

	assert_eq!(report.gate_drops, 0, "missing map-named PDU became a denial");
	assert_eq!(
		report.fallback.as_deref(),
		Some("unevaluable"),
		"missing map-named PDU used the wrong fallback",
	);

	assert_eq!(report.state_len, None, "missing map-named PDU produced state");
	assert_no_memo(services, fork.event_id.as_ref()).await?;
	assert_fetches(
		services,
		&room_id,
		&top,
		top_json,
		ExpectedWalkOutcome::Unevaluable,
		"missing map-named PDU",
	)
	.await
}

async fn auth_ancestor_failure(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
	failure: AncestorFailure,
) -> Result {
	let pdu_failure = match failure {
		| AncestorFailure::Malformed => PduFailure::Malformed,
		| AncestorFailure::Missing | AncestorFailure::InteriorFork => PduFailure::Missing,
	};

	let interior = matches!(failure, AncestorFailure::InteriorFork);
	let room_id = create_room(services, base, token).await?;
	let ancestor = verified_replaced_membership_ancestor(services, &room_id, user_id).await?;
	let (left, right, fork, fork_json) = held_state_fork(services, user_id, &room_id).await?;
	let (top, top_json) = if interior {
		set_forward_extremity(services, &room_id, fork.event_id.as_ref()).await;

		let (top, top_json) = sign_message(services, user_id, &room_id, "sentinel top").await?;

		services
			.timeline
			.add_pdu_outlier(&top.event_id, &top_json);

		(top, top_json)
	} else {
		(fork.clone(), fork_json)
	};

	corrupt_timeline_pdu(services, &ancestor, pdu_failure).await?;
	suppress_upgrade(services, left.event_id.as_ref())?;
	suppress_upgrade(services, right.event_id.as_ref())?;

	if interior {
		suppress_upgrade(services, fork.event_id.as_ref())?;
	}

	let context = match failure {
		| AncestorFailure::Missing => "missing auth ancestor",
		| AncestorFailure::Malformed => "malformed auth ancestor",
		| AncestorFailure::InteriorFork => "interior fork sentinel",
	};

	assert_unevaluable(services, top.event_id.as_ref(), context).await?;

	if interior {
		assert_no_memo(services, fork.event_id.as_ref()).await?;
	}

	assert_fetches(services, &room_id, &top, top_json, ExpectedWalkOutcome::Unevaluable, context)
		.await?;

	Ok(())
}

async fn corrupt_chain_cache_rebuilds(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let membership = services
		.state_accessor
		.room_state_get(&room_id, &StateEventType::RoomMember, user_id.as_str())
		.await?;

	let room_version = services.state.get_room_version(&room_id).await?;
	let shorteventid = services
		.short
		.get_shorteventid(membership.event_id.as_ref())
		.await?;

	let key = serialize_key([shorteventid].as_slice())?;

	services.clear_cache().await;

	let first_complete = AtomicBool::new(true);
	let mut expected = services
		.auth_chain
		.event_ids_iter_strict(
			&room_id,
			&room_version,
			once(membership.event_id.as_ref()),
			&first_complete,
		)
		.try_collect::<Vec<_>>()
		.await?;

	assert!(first_complete.load(Ordering::Relaxed), "initial auth chain was incomplete");
	assert!(!expected.is_empty(), "auth-chain cache fixture has an empty chain");

	let cache = services.db.get("authchainkey_authchain")?;

	cache
		.exists(&key)
		.await
		.map_err(|error| err!("initial auth-chain cache row was not written: {error}"))?;

	cache.insert(&key, b"!");

	let complete = AtomicBool::new(true);
	let mut rebuilt = services
		.auth_chain
		.event_ids_iter_strict(
			&room_id,
			&room_version,
			once(membership.event_id.as_ref()),
			&complete,
		)
		.try_collect::<Vec<_>>()
		.await?;

	expected.sort_unstable();
	rebuilt.sort_unstable();

	assert_eq!(rebuilt, expected, "malformed auth-chain cache did not rebuild");
	assert!(complete.load(Ordering::Relaxed), "cache rebuild tripped completeness");

	let rebuilt = cache.get(&key).await?;

	assert!(
		rebuilt.len().is_multiple_of(size_of::<u64>()),
		"rebuilt auth-chain cache remains malformed"
	);

	assert_ne!(&*rebuilt, b"!", "malformed auth-chain cache was not replaced");

	Ok(())
}

async fn unpolled_chain_stays_clear(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let ancestor = verified_replaced_membership_ancestor(services, &room_id, user_id).await?;
	let (left, right, top, top_json) = held_message_fork(services, user_id, &room_id).await?;

	corrupt_timeline_pdu(services, &ancestor, PduFailure::Missing).await?;
	suppress_upgrade(services, left.event_id.as_ref())?;
	suppress_upgrade(services, right.event_id.as_ref())?;

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	assert_eq!(report.forks, 1, "unpolled fixture missed its fork");
	assert_eq!(report.gate_drops, 0, "unpolled chain became a denial");
	assert_eq!(report.fallback, None, "unpolled chain tripped the sentinel");
	assert!(report.state_len.is_some(), "unpolled chain produced no state");
	assert_accepts(services, &room_id, &top, top_json, "unpolled chain").await
}

async fn direct_memo_failure_is_miss(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let boundary = append_message(services, user_id, &room_id, "direct memo boundary").await?;
	let (held, top, top_json) =
		held_message_chain(services, user_id, &room_id, &boundary).await?;

	services.clear_cache().await;
	suppress_upgrade(services, held.event_id.as_ref())?;
	plant_memo(services, top.event_id.as_ref(), ShortStateHash::MAX).await?;

	let memo = services.db.get("eventid_resolvedstate")?;

	memo.exists(&top.event_id)
		.await
		.map_err(|error| err!("direct memo fixture was not planted: {error}"))?;

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	assert_eq!(report.memo_hits, 0, "direct memo failure entered the walk");
	assert_eq!(report.gate_drops, 0, "direct memo failure became a denial");
	assert_eq!(report.fallback, None, "direct memo failure triggered fallback");
	assert!(report.state_len.is_some(), "direct memo failure produced no state");

	assert_accepts(services, &room_id, &top, top_json, "direct memo failure").await
}

async fn walk_memo_failure_is_unevaluable(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let boundary = append_message(services, user_id, &room_id, "walk memo boundary").await?;
	let (memo, middle, _) = held_message_chain(services, user_id, &room_id, &boundary).await?;

	set_forward_extremity(services, &room_id, middle.event_id.as_ref()).await;

	let (top, top_json) = sign_message(services, user_id, &room_id, "walk memo top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	services.clear_cache().await;
	suppress_upgrade(services, memo.event_id.as_ref())?;
	suppress_upgrade(services, middle.event_id.as_ref())?;
	plant_memo(services, memo.event_id.as_ref(), ShortStateHash::MAX).await?;

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	assert_eq!(report.memo_hits, 1, "walk memo was not materialized");
	assert_eq!(report.gate_drops, 0, "walk memo failure became a denial");
	assert_eq!(
		report.fallback.as_deref(),
		Some("unevaluable"),
		"walk memo failure used the wrong fallback",
	);
	assert_eq!(report.state_len, None, "walk memo failure produced state");
	assert_no_memo(services, middle.event_id.as_ref()).await?;

	assert_fetches(
		services,
		&room_id,
		&top,
		top_json,
		ExpectedWalkOutcome::Unevaluable,
		"walk memo failure",
	)
	.await
}

async fn degree_one_state_miss(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let anchor = append_message(services, user_id, &room_id, "degree one anchor").await?;
	let intact_state = services.state.pdu_shortstatehash(&anchor).await?;
	append_state(services, user_id, &room_id, "degree one change").await?;
	let boundary = append_message(services, user_id, &room_id, "degree one boundary").await?;
	let (incoming, incoming_json) =
		sign_message(services, user_id, &room_id, "degree one top").await?;

	let corrupt_state = services
		.state
		.pdu_shortstatehash(&boundary)
		.await?;

	services
		.timeline
		.add_pdu_outlier(&incoming.event_id, &incoming_json);

	assert_ne!(intact_state, corrupt_state, "degree one fixture reused the intact state");
	restore_room_state(services, &room_id, intact_state, &anchor).await;
	remove_short_row(services, "shortstatehash_statediff", corrupt_state).await?;
	assert_eq!(
		services
			.state
			.get_room_shortstatehash(&room_id)
			.await?,
		intact_state,
		"degree one fixture did not restore the current room state",
	);
	services
		.state_accessor
		.state_full_ids_strict(intact_state)
		.try_collect::<Vec<_>>()
		.await
		.map_err(|error| err!("degree one fixture corrupted the restored state: {error}"))?;
	assert_all_committed(services, incoming.event_id.as_ref(), "degree one state miss").await?;
	assert_fetches(
		services,
		&room_id,
		&incoming,
		incoming_json,
		ExpectedWalkOutcome::AllCommitted,
		"degree one state miss",
	)
	.await
}

async fn sibling_state_miss(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let boundary = append_message(services, user_id, &room_id, "sibling boundary").await?;
	let boundary_state = services
		.state
		.pdu_shortstatehash(&boundary)
		.await?;
	append_state(services, user_id, &room_id, "sibling left change").await?;
	let left = append_message(services, user_id, &room_id, "sibling left").await?;
	let state_lock = services.state.mutex.lock(&room_id).await;

	services
		.state
		.set_room_state(&room_id, boundary_state, &state_lock);

	services
		.state
		.set_forward_extremities(&room_id, once(boundary.as_ref()), &state_lock)
		.await;

	drop(state_lock);

	let right = append_message(services, user_id, &room_id, "sibling right").await?;

	set_forward_extremities(services, &room_id, [left.as_ref(), right.as_ref()]).await;

	let (incoming, incoming_json) =
		sign_message(services, user_id, &room_id, "sibling top").await?;

	let left_state = services.state.pdu_shortstatehash(&left).await?;
	let right_state = services.state.pdu_shortstatehash(&right).await?;

	assert_ne!(left_state, right_state, "sibling fixture states did not diverge");

	services
		.timeline
		.add_pdu_outlier(&incoming.event_id, &incoming_json);

	remove_short_row(services, "shortstatehash_statediff", left_state).await?;
	assert_all_committed(services, incoming.event_id.as_ref(), "sibling state miss").await?;
	assert_fetches(
		services,
		&room_id,
		&incoming,
		incoming_json,
		ExpectedWalkOutcome::AllCommitted,
		"sibling state miss",
	)
	.await
}

async fn held_message_chain(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	boundary: &EventId,
) -> Result<(PduEvent, PduEvent, CanonicalJsonObject)> {
	set_forward_extremity(services, room_id, boundary).await;

	let (held, held_json) = sign_message(services, user_id, room_id, "held corruption").await?;

	services
		.timeline
		.add_pdu_outlier(&held.event_id, &held_json);

	set_forward_extremity(services, room_id, held.event_id.as_ref()).await;

	let (top, top_json) = sign_message(services, user_id, room_id, "corruption top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	Ok((held, top, top_json))
}

async fn held_state_fork(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<(PduEvent, PduEvent, PduEvent, CanonicalJsonObject)> {
	let (left, left_json) = sign_state(services, user_id, room_id, "fork left").await?;
	let (right, right_json) = sign_state(services, user_id, room_id, "fork right").await?;

	services
		.timeline
		.add_pdu_outlier(&left.event_id, &left_json);

	services
		.timeline
		.add_pdu_outlier(&right.event_id, &right_json);

	set_forward_extremities(services, room_id, [left.event_id.as_ref(), right.event_id.as_ref()])
		.await;

	let (top, top_json) = sign_message(services, user_id, room_id, "fork top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	Ok((left, right, top, top_json))
}

async fn held_message_fork(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<(PduEvent, PduEvent, PduEvent, CanonicalJsonObject)> {
	let (left, left_json) = sign_message(services, user_id, room_id, "plain fork left").await?;
	let (right, right_json) =
		sign_message(services, user_id, room_id, "plain fork right").await?;

	services
		.timeline
		.add_pdu_outlier(&left.event_id, &left_json);

	services
		.timeline
		.add_pdu_outlier(&right.event_id, &right_json);

	set_forward_extremities(services, room_id, [left.event_id.as_ref(), right.event_id.as_ref()])
		.await;

	let (top, top_json) = sign_message(services, user_id, room_id, "plain fork top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	Ok((left, right, top, top_json))
}

async fn verified_replaced_membership_ancestor(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
) -> Result<OwnedEventId> {
	let ancestor = services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())
		.await?;
	let content = RoomMemberEventContent::new(MembershipState::Join);
	let builder = PduBuilder::state(user_id.to_string(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;
	let membership = services
		.timeline
		.build_and_append_pdu(builder, user_id, room_id, &state_lock)
		.await?;
	drop(state_lock);
	let current = services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())
		.await?;

	if current.event_id != membership {
		return Err!("replacement membership did not become current room state");
	}

	let room_version = services.state.get_room_version(room_id).await?;
	let has_ancestor = services
		.auth_chain
		.event_ids_iter(room_id, &room_version, once(membership.as_ref()))
		.try_any(|event_id| ready(event_id == ancestor.event_id))
		.await?;

	if !has_ancestor {
		return Err!("replaced membership is not an auth ancestor of its successor");
	}

	Ok(ancestor.event_id.clone())
}

async fn append_state(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	name: &str,
) -> Result<OwnedEventId> {
	let content = RoomNameEventContent::new(name.to_owned());
	let builder = PduBuilder::state(String::new(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(builder, user_id, room_id, &state_lock)
		.await
}

async fn remove_short_row(services: &Services, map_name: &str, short: u64) -> Result {
	let map = services
		.db
		.get(map_name)
		.map_err(|error| err!("short-id map {map_name} unavailable for {short}: {error}"))?;

	let key = short.to_be_bytes();

	map.exists(&key)
		.await
		.map_err(|error| err!("short-id row {map_name}[{short}] unavailable: {error}"))?;

	map.remove(&key);
	services.clear_cache().await;

	assert!(
		map.exists(&key)
			.await
			.is_err_and(|error| error.is_not_found()),
		"raw short-id mutation left {map_name}[{short}] readable"
	);

	Ok(())
}

async fn corrupt_timeline_pdu(
	services: &Services,
	event_id: &EventId,
	failure: PduFailure,
) -> Result {
	let pdu_id = services
		.timeline
		.get_pdu_id(event_id)
		.await
		.map_err(|error| err!("timeline PDU {event_id} has no raw id: {error}"))?;

	let pdus = services
		.db
		.get("pduid_pdu")
		.map_err(|error| err!("timeline PDU map unavailable for {event_id}: {error}"))?;

	pdus.exists(&pdu_id)
		.await
		.map_err(|error| err!("timeline PDU row unavailable for {event_id}: {error}"))?;

	let failure_name = match failure {
		| PduFailure::Missing => {
			pdus.remove(&pdu_id);
			"missing"
		},
		| PduFailure::Malformed => {
			pdus.insert(&pdu_id, b"{");
			"malformed"
		},
	};

	services.clear_cache().await;

	let result = services.timeline.get_pdu_from_id(&pdu_id).await;
	let Err(error) = result else {
		return Err!("{failure_name} timeline PDU {event_id} remained readable");
	};

	match failure {
		| PduFailure::Missing => assert!(
			error.is_not_found(),
			"missing timeline PDU {event_id} returned an unexpected error: {error}"
		),
		| PduFailure::Malformed => assert!(
			matches!(&error, Error::Json(_)),
			"malformed timeline PDU {event_id} returned an unexpected error: {error}"
		),
	}

	Ok(())
}

async fn plant_memo(
	services: &Services,
	event_id: &EventId,
	shortstatehash: ShortStateHash,
) -> Result {
	let memo = services
		.db
		.get("eventid_resolvedstate")
		.map_err(|error| err!("resolved-state memo map unavailable for {event_id}: {error}"))?;

	memo.raw_aput::<{ size_of::<ShortStateHash>() }, _, _>(event_id.as_bytes(), shortstatehash);

	let stored: ShortStateHash = memo
		.get(event_id)
		.await
		.deserialized()
		.map_err(|error| err!("resolved-state memo for {event_id} was unreadable: {error}"))?;

	assert_eq!(
		stored, shortstatehash,
		"resolved-state memo for {event_id} stored the wrong state hash"
	);

	Ok(())
}

async fn assert_unevaluable(services: &Services, event_id: &EventId, context: &str) -> Result {
	let report = services
		.event_handler
		.local_state_report(event_id)
		.await?;

	assert!(report.visited > 0, "{context} did not exercise the local walk");
	assert_eq!(report.gate_drops, 0, "{context} became a denial");
	assert_eq!(
		report.fallback.as_deref(),
		Some("unevaluable"),
		"{context} used the wrong fallback",
	);
	assert_eq!(report.state_len, None, "{context} produced state");

	Ok(())
}

async fn assert_all_committed(services: &Services, event_id: &EventId, context: &str) -> Result {
	let report = services
		.event_handler
		.local_state_report(event_id)
		.await?;

	assert_eq!(report.visited, 0, "{context} unexpectedly walked a held event");
	assert_eq!(report.gate_drops, 0, "{context} became a denial");
	assert_eq!(
		report.fallback.as_deref(),
		Some("all_committed"),
		"{context} used the wrong fallback",
	);
	assert_eq!(report.state_len, None, "{context} produced state");

	Ok(())
}

async fn assert_no_memo(services: &Services, event_id: &EventId) -> Result {
	let memo = services.db.get("eventid_resolvedstate")?;

	assert!(
		memo.exists(event_id)
			.await
			.is_err_and(|error| error.is_not_found()),
		"failed fork {event_id} wrote a resolved-state memo"
	);

	Ok(())
}

async fn assert_fetches(
	services: &Services,
	room_id: &RoomId,
	incoming: &PduEvent,
	incoming_json: CanonicalJsonObject,
	expected: ExpectedWalkOutcome,
	context: &str,
) -> Result {
	let room_version = match services.state.get_room_version(room_id).await {
		| Ok(room_version) => room_version,
		| Err(error) => return Err!("{context} failed to load the room version: {error}"),
	};

	let incoming_json = into_outgoing_federation(incoming_json, &room_version);
	let before = services.event_handler.state_local_metrics();

	let result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			incoming.event_id.as_ref(),
			incoming_json,
			true,
		)
		.await;

	let after = services.event_handler.state_local_metrics();

	assert_one_settled_walk(before, after, expected, context);

	let Err(error) = result else {
		return Err!("{context} did not fall through to federation fetch");
	};

	if !error
		.to_string()
		.contains("no candidate servers available")
	{
		return Err!("{context} failed before federation fetch: {error}");
	}

	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(incoming.event_id.as_ref())
			.await
			.is_err_and(|error| error.is_not_found()),
		"{context} unexpectedly reached the timeline"
	);

	assert!(
		services
			.timeline
			.pdu_exists(incoming.event_id.as_ref())
			.await,
		"{context} was not retained as an outlier"
	);

	Ok(())
}

#[derive(Clone, Copy)]
enum ExpectedWalkOutcome {
	Resolved,
	AllCommitted,
	Unevaluable,
}

fn assert_one_settled_walk(
	before: StateLocalMetrics,
	after: StateLocalMetrics,
	expected: ExpectedWalkOutcome,
	context: &str,
) {
	let actual = walk_metrics_delta(&before, &after, context);
	let expected = expected_walk_metrics(expected);

	assert_eq!(actual, expected, "{context} used the wrong local walk outcome");
	assert_eq!(settled_walks(&actual), 1, "{context} did not settle exactly once");
}

fn walk_metrics_delta(
	before: &StateLocalMetrics,
	after: &StateLocalMetrics,
	context: &str,
) -> StateLocalMetrics {
	StateLocalMetrics {
		walk_attempts: counter_delta(after.walk_attempts, before.walk_attempts, context),
		walk_resolved: counter_delta(after.walk_resolved, before.walk_resolved, context),
		fallback_absent: counter_delta(after.fallback_absent, before.fallback_absent, context),
		fallback_ceiling: counter_delta(after.fallback_ceiling, before.fallback_ceiling, context),
		fallback_auth_missing: counter_delta(
			after.fallback_auth_missing,
			before.fallback_auth_missing,
			context,
		),
		fallback_all_committed: counter_delta(
			after.fallback_all_committed,
			before.fallback_all_committed,
			context,
		),
		fallback_entries: counter_delta(after.fallback_entries, before.fallback_entries, context),
		fallback_canary: counter_delta(after.fallback_canary, before.fallback_canary, context),
		fallback_create_mismatch: counter_delta(
			after.fallback_create_mismatch,
			before.fallback_create_mismatch,
			context,
		),
		fallback_unevaluable: counter_delta(
			after.fallback_unevaluable,
			before.fallback_unevaluable,
			context,
		),
		fallback_error: counter_delta(after.fallback_error, before.fallback_error, context),
		walk_failures: counter_delta(after.walk_failures, before.walk_failures, context),
		..StateLocalMetrics::default()
	}
}

fn counter_delta(after: u64, before: u64, context: &str) -> u64 {
	after
		.checked_sub(before)
		.unwrap_or_else(|| panic!("{context} local walk counter decreased"))
}

fn expected_walk_metrics(outcome: ExpectedWalkOutcome) -> StateLocalMetrics {
	match outcome {
		| ExpectedWalkOutcome::Resolved => StateLocalMetrics {
			walk_attempts: 1,
			walk_resolved: 1,
			..StateLocalMetrics::default()
		},
		| ExpectedWalkOutcome::AllCommitted => StateLocalMetrics {
			walk_attempts: 1,
			fallback_all_committed: 1,
			..StateLocalMetrics::default()
		},
		| ExpectedWalkOutcome::Unevaluable => StateLocalMetrics {
			walk_attempts: 1,
			fallback_unevaluable: 1,
			..StateLocalMetrics::default()
		},
	}
}

fn settled_walks(metrics: &StateLocalMetrics) -> u64 {
	[
		metrics.walk_resolved,
		metrics.fallback_absent,
		metrics.fallback_ceiling,
		metrics.fallback_auth_missing,
		metrics.fallback_all_committed,
		metrics.fallback_entries,
		metrics.fallback_canary,
		metrics.fallback_create_mismatch,
		metrics.fallback_unevaluable,
		metrics.fallback_error,
		metrics.walk_failures,
	]
	.into_iter()
	.sum()
}

async fn assert_accepts(
	services: &Services,
	room_id: &RoomId,
	incoming: &PduEvent,
	incoming_json: CanonicalJsonObject,
	context: &str,
) -> Result {
	let room_version = match services.state.get_room_version(room_id).await {
		| Ok(room_version) => room_version,
		| Err(error) => return Err!("{context} failed to load the room version: {error}"),
	};

	let incoming_json = into_outgoing_federation(incoming_json, &room_version);

	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(incoming.event_id.as_ref())
			.await
			.is_err_and(|error| error.is_not_found()),
		"{context} unexpectedly started in the timeline"
	);

	let result = match services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			incoming.event_id.as_ref(),
			incoming_json,
			true,
		)
		.await
	{
		| Ok(result) => result,
		| Err(error) => return Err!("{context} failed to handle the incoming PDU: {error}"),
	};

	assert!(result.is_some(), "{context} did not continue through local state");
	match services
		.timeline
		.non_outlier_pdu_exists(incoming.event_id.as_ref())
		.await
	{
		| Ok(()) => Ok(()),
		| Err(error) => Err!("{context} did not reach the timeline: {error}"),
	}
}

async fn set_forward_extremities<const N: usize>(
	services: &Services,
	room_id: &RoomId,
	event_ids: [&EventId; N],
) {
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_forward_extremities(room_id, event_ids.into_iter(), &state_lock)
		.await;
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

	let before_report = services.event_handler.state_local_metrics();

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	let after_report = services.event_handler.state_local_metrics();

	assert_eq!(after_report, before_report, "local state diagnostic changed production metrics");

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

	let before = services.event_handler.state_local_metrics();

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

	let after = services.event_handler.state_local_metrics();

	assert_one_settled_walk(before, after, ExpectedWalkOutcome::Resolved, "held multi-prev fork");
	assert_eq!(
		after
			.walk_resolved
			.checked_sub(before.walk_resolved)
			.expect("walk resolved counter should not decrease"),
		1,
		"held multi-prev fork did not resolve locally",
	);

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
	let (denied_left, denied_left_json) =
		sign_state(services, user_id, room_id, "denied left").await?;

	let (denied_right, denied_right_json) =
		sign_state(services, user_id, room_id, "denied right").await?;

	replace_state_before_without(
		services,
		room_id,
		&base,
		&StateEventType::RoomMember,
		user_id.as_str(),
	)
	.await?;

	let room_version = services.state.get_room_version(room_id).await?;

	for (denied, denied_json) in
		[(&denied_left, denied_left_json), (&denied_right, denied_right_json)]
	{
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
	}

	set_forward_extremities(services, room_id, [
		denied_left.event_id.as_ref(),
		denied_right.event_id.as_ref(),
	])
	.await;

	let (top, top_json) = sign_message(services, user_id, room_id, "denial top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	let before_report = services.event_handler.state_local_metrics();

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	let after_report = services.event_handler.state_local_metrics();

	assert_eq!(after_report, before_report, "local state diagnostic changed production metrics");

	assert_eq!(report.visited, 2, "local traversal missed a denied event");
	assert_eq!(report.forks, 1, "local traversal missed the denied fork");
	assert_eq!(report.gate_drops, 2, "gate denials were not counted exactly once each");
	assert_eq!(report.fallback, None, "clean gate denial triggered a fetch");
	assert!(report.state_len.is_some(), "clean gate denial lost the built state");

	let before = services.event_handler.state_local_metrics();
	let top_json = into_outgoing_federation(top_json, &room_version);
	let is_timeline_event = true;

	let result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			top.event_id.as_ref(),
			top_json,
			is_timeline_event,
		)
		.await;

	assert!(
		matches!(&result, Err(Error::AuthCheck(..))),
		"event over denied membership had an unexpected result: {result:?}",
	);

	let after = services.event_handler.state_local_metrics();

	assert_one_settled_walk(before, after, ExpectedWalkOutcome::Resolved, "clean gate denial");
	assert_eq!(
		after
			.walk_resolved
			.checked_sub(before.walk_resolved)
			.expect("walk resolved counter should not decrease"),
		1,
		"clean gate denial did not resolve locally",
	);
	assert_eq!(
		after
			.gate_denials
			.checked_sub(before.gate_denials)
			.expect("gate denial counter should not decrease"),
		u64::try_from(report.gate_drops).expect("gate denial count should fit in u64"),
		"clean gate denials were not aggregated exactly once each",
	);

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

async fn restore_room_state(
	services: &Services,
	room_id: &RoomId,
	shortstatehash: ShortStateHash,
	event_id: &EventId,
) {
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_room_state(room_id, shortstatehash, &state_lock);

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
