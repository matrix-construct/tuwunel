#![allow(clippy::expect_used)]
#![allow(clippy::tests_outside_test_module)]

use std::{fs::remove_dir_all, net::TcpListener, time::Duration};

use futures::future::{BoxFuture, join};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Error, Result, async_noinline, err, ruma::UserId};
use tuwunel_service::{Services, users::Register};

use self::{
	auth_chain::{
		AncestorFailure, ancestor_failure, corrupt_chain_cache_rebuilds,
		current_state_auth_failure, unpolled_chain_stays_clear,
	},
	baseline::enabled_baseline,
	disabled::ignores_planted_memo,
	helpers::{create_room, walks_in_flight},
	memo::{direct_memo_failure_is_miss, walk_memo_failure_is_unevaluable},
	missing_rows::{
		missing_event_reverse, missing_named_pdu, missing_state_diff, missing_state_key_reverse,
	},
	state_miss::{degree_one_state_miss, sibling_state_miss},
};

#[path = "state_local_build/auth_chain.rs"]
mod auth_chain;
#[path = "state_local_build/baseline.rs"]
mod baseline;
#[path = "state_local_build/disabled.rs"]
mod disabled;
#[path = "state_local_build/helpers.rs"]
mod helpers;
#[path = "state_local_build/memo.rs"]
mod memo;
#[path = "state_local_build/missing_rows.rs"]
mod missing_rows;
#[path = "state_local_build/positional.rs"]
mod positional;
#[path = "state_local_build/prev_walk.rs"]
mod prev_walk;
#[path = "state_local_build/redelivery.rs"]
mod redelivery;
#[path = "state_local_build/soft_fail.rs"]
mod soft_fail;
#[path = "state_local_build/state_miss.rs"]
mod state_miss;

#[derive(Clone, Copy)]
enum Case {
	Disabled,
	MaxDisabled,
	LegacyDisabled,
	Baseline,
	MissingStateDiff,
	MissingEventReverse,
	MissingStateKeyReverse,
	MissingNamedPdu,
	MissingAuthAncestor,
	CorruptChainCache,
	DirectMemoFailure,
	WalkMemoFailure,
	DegreeOneStateMiss,
	SiblingStateMiss,
	UnpolledChain,
	InteriorForkSentinel,
	V12ResolverFailure,
	CurrentStateAuthFailure,
}

const CASES: [Case; 18] = [
	Case::Disabled,
	Case::MaxDisabled,
	Case::LegacyDisabled,
	Case::Baseline,
	Case::MissingStateDiff,
	Case::MissingEventReverse,
	Case::MissingStateKeyReverse,
	Case::MissingNamedPdu,
	Case::MissingAuthAncestor,
	Case::CorruptChainCache,
	Case::DirectMemoFailure,
	Case::WalkMemoFailure,
	Case::DegreeOneStateMiss,
	Case::SiblingStateMiss,
	Case::UnpolledChain,
	Case::InteriorForkSentinel,
	Case::V12ResolverFailure,
	Case::CurrentStateAuthFailure,
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

	let db_path = Args::test_database_path(format_args!("state-local-build-{name}"));
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_database_path(&db_path)
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("log_enable=false")
		.with_option(format!("resolve_state_locally={}", !matches!(case, Case::Disabled)));

	let args = match case {
		| Case::MaxDisabled => args.with_option("resolve_state_locally_max=0"),
		| Case::LegacyDisabled => args.with_option("resolve_state_locally_shadow=true"),
		| _ => args,
	};

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
		| Case::MaxDisabled => "max-disabled",
		| Case::LegacyDisabled => "legacy-disabled",
		| Case::Baseline => "baseline",
		| Case::MissingStateDiff => "missing-state-diff",
		| Case::MissingEventReverse => "missing-event-reverse",
		| Case::MissingStateKeyReverse => "missing-state-key-reverse",
		| Case::MissingNamedPdu => "missing-named-pdu",
		| Case::MissingAuthAncestor => "missing-auth-ancestor",
		| Case::CorruptChainCache => "corrupt-chain-cache",
		| Case::DirectMemoFailure => "direct-memo-failure",
		| Case::WalkMemoFailure => "walk-memo-failure",
		| Case::DegreeOneStateMiss => "degree-one-state-miss",
		| Case::SiblingStateMiss => "sibling-state-miss",
		| Case::UnpolledChain => "unpolled-chain",
		| Case::InteriorForkSentinel => "interior-fork-sentinel",
		| Case::V12ResolverFailure => "v12-resolver-failure",
		| Case::CurrentStateAuthFailure => "current-state-auth-failure",
	}
}

// size firewall
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

	exercise_case(services, base, token, &user_id, case).await?;

	let in_flight = walks_in_flight(services);

	match in_flight {
		| 0 => Ok(()),
		| _ => Err!("a prev walk outlived its case, {in_flight} still in flight"),
	}
}

async fn wait_until_ready(services: &Services, base: &str) -> Result {
	let url = format!("{base}/_matrix/client/versions");
	let probe = || services.client.clients.default.get(&url).send();

	timeout(Duration::from_secs(10), async {
		while probe().await.is_err() {
			sleep(Duration::from_millis(20)).await;
		}
	})
	.await
	.map_err(|_| err!("server listener did not become ready"))
}

fn exercise_case<'a>(
	services: &'a Services,
	base: &'a str,
	token: &'a str,
	user_id: &'a UserId,
	case: Case,
) -> BoxFuture<'a, Result> {
	// size firewall
	match case {
		| Case::Baseline => Box::pin(enabled_baseline(services, base, token, user_id)),
		| Case::MissingStateDiff => Box::pin(missing_state_diff(services, base, token, user_id)),
		| Case::MissingEventReverse =>
			Box::pin(missing_event_reverse(services, base, token, user_id)),
		| Case::MissingStateKeyReverse =>
			Box::pin(missing_state_key_reverse(services, base, token, user_id)),
		| Case::MissingNamedPdu => Box::pin(missing_named_pdu(services, base, token, user_id)),
		| Case::MissingAuthAncestor =>
			Box::pin(ancestor_failure(services, base, token, user_id, AncestorFailure::Missing)),
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
		| Case::InteriorForkSentinel => Box::pin(ancestor_failure(
			services,
			base,
			token,
			user_id,
			AncestorFailure::InteriorFork,
		)),
		| Case::V12ResolverFailure => Box::pin(ancestor_failure(
			services,
			base,
			token,
			user_id,
			AncestorFailure::V12InteriorFork,
		)),
		| Case::CurrentStateAuthFailure =>
			Box::pin(current_state_auth_failure(services, base, token, user_id)),
		| Case::Disabled | Case::MaxDisabled | Case::LegacyDisabled => Box::pin(async move {
			let room_id = create_room(services, base, token).await?;

			ignores_planted_memo(services, user_id, &room_id).await
		}),
	}
}
