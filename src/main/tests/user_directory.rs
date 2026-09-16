#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use serde_json::json;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Result, err, itertools::Itertools, ruma::UserId, utils::BoolExt};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const HOST: &str = "127.0.0.1";

const SEARCHER_TOKEN: &str = "directory-searcher-test-token-0123456789abcdef";
const AGENT_TOKEN: &str = "directory-agent-test-token-0123456789abcdef";

const HUMAN: &str = "@directory_human:localhost";
const AGENT: &str = "@directory_agent:localhost";
const SENDER: &str = "@directory_sender:localhost";

/// One visibility case for the prefix search.
///
/// The two knobs set the server config; `visible` and `visible_shared` list
/// the accounts the search returns before and after the agent shares a room
/// with the searcher.
struct Case {
	name: &'static str,
	show_appservices: Option<bool>,
	show_all: bool,
	visible: &'static [&'static str],
	visible_shared: &'static [&'static str],
}

const CASES: [Case; 4] = [
	Case {
		name: "hidden",
		show_appservices: None,
		show_all: false,
		visible: &[],
		visible_shared: &[],
	},
	Case {
		name: "default",
		show_appservices: None,
		show_all: true,
		visible: &[HUMAN],
		visible_shared: &[HUMAN],
	},
	Case {
		name: "enabled",
		show_appservices: Some(true),
		show_all: true,
		visible: &[AGENT, HUMAN, SENDER],
		visible_shared: &[AGENT, HUMAN, SENDER],
	},
	Case {
		name: "rooms",
		show_appservices: Some(true),
		show_all: false,
		visible: &[],
		visible_shared: &[AGENT],
	},
];

/// Drives `show_appservice_users_in_user_directory` end to end over the
/// client API.
///
/// Each case boots its own server on its own port and database. They run in
/// sequence with logging off, since the global tracing subscriber installs
/// once per process.
#[test]
fn appservice_users_follow_the_directory_knob() -> Result { CASES.iter().try_for_each(run_case) }

fn run_case(case: &Case) -> Result {
	let name = case.name;
	let listener = TcpListener::bind((HOST, 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option(format!("address=[\"{HOST}\"]"))
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("log_enable=false")
		.with_option(format!("show_all_local_users_in_user_directory={}", case.show_all));

	let args = case
		.show_appservices
		.map(|enabled| format!("show_appservice_users_in_user_directory={enabled}"))
		.into_iter()
		.fold(args, Args::with_option);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://{HOST}:{port}");

		drop(listener);

		let drive = async {
			let outcome = exercise(&services, &base, case).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run_result, outcome) = join(async_run(&server), drive).await;

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	result.map_err(|error| err!("directory case {name} failed: {error}"))
}

async fn exercise(services: &Services, base: &str, case: &Case) -> Result {
	wait_until_ready(services, base).await?;
	let searcher = register(services, "searcher", SEARCHER_TOKEN).await?;
	let human: &UserId = HUMAN.try_into()?;

	services
		.users
		.create(human, Some("password"), None)
		.await?;

	let registration = json!({
		"id": "directory-test",
		"url": null,
		"as_token": "directory-test-appservice-token-0123456789",
		"hs_token": "directory-test-homeserver-token-0123456789",
		"sender_localpart": "directory_sender",
		"namespaces": {
			"users": [{"exclusive": true, "regex": format!("^{AGENT}$")}],
			"aliases": [],
			"rooms": []
		}
	});

	services
		.appservice
		.register_appservice(serde_json::from_value(registration)?)
		.await?;

	let agent: &UserId = AGENT.try_into()?;

	services.users.create(agent, None, None).await?;
	services
		.users
		.create_device(agent, None, (Some(AGENT_TOKEN), None), None, None, None)
		.await?;

	assert_exclusive(services, agent, "did not register the agent into an exclusive namespace")
		.await?;

	let client = Client { services, base, token: SEARCHER_TOKEN };

	assert_visible(&client, "directory_", case.visible).await?;
	assert_visible(&client, "no-matching-account", &[]).await?;
	assert_visible(&client, searcher.as_str(), &[]).await?;

	let agent_client = Client { token: AGENT_TOKEN, ..client };
	let room_id = agent_client
		.create_room(&json!({"preset": "private_chat", "invite": [searcher]}))
		.await?;

	client
		.post(&format!("rooms/{room_id}/join"), &json!({}))
		.await?;

	assert_visible(&client, "directory_", case.visible_shared).await?;
	assert_exclusive(services, agent, "lost the agent's exclusive namespace").await
}

async fn assert_exclusive(services: &Services, agent: &UserId, failure: &str) -> Result {
	let exclusive = services
		.appservice
		.is_exclusive_user_id(agent)
		.await;

	exclusive
		.into_option()
		.ok_or_else(|| err!("{failure}"))
}

async fn assert_visible(client: &Client<'_>, term: &str, expected: &[&str]) -> Result {
	let body = client
		.post("user_directory/search", &json!({"search_term": term, "limit": 100}))
		.await?;

	let results = body["results"]
		.as_array()
		.ok_or_else(|| err!("search for {term} omitted results"))?;

	let users: Vec<_> = results
		.iter()
		.map(|user| field(user, "user_id"))
		.process_results(|users| users.sorted_unstable().collect())?;

	if users != expected {
		return Err!("search for {term} returned {users:?}, expected {expected:?}");
	}

	if body["limited"] != false {
		return Err!("search for {term} reported limited results");
	}

	Ok(())
}
