#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Result, ruma::user_id};
use tuwunel_service::Services;

use self::client::{Client, register, wait_until_ready};

#[path = "../client/mod.rs"]
mod client;

const TOKEN: &str = "directory-search-test-token-0123456789abcdef";
const AGENT_TOKEN: &str = "directory-agent-test-token-0123456789abcdef";

pub(super) fn check_directory(
	show_appservices: Option<bool>,
	show_all: bool,
	expected: &[&str],
) -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let mut args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option(format!("show_all_local_users_in_user_directory={show_all}"));
	if let Some(enabled) = show_appservices {
		args.option
			.push(format!("show_appservice_users_in_user_directory={enabled}"));
	}

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");
		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base, show_appservices, show_all, expected).await;
			let shutdown = server.server.shutdown();
			outcome.and(shutdown)
		};
		let (run_result, outcome) = join(async_run(&server), exercise).await;
		drop(services);
		async_stop(&server).await?;
		run_result?;
		outcome
	})
}

async fn exercise(
	services: &Services,
	base: &str,
	show_appservices: Option<bool>,
	show_all: bool,
	expected: &[&str],
) -> Result {
	wait_until_ready(services, base).await?;
	let requester = register(services, "searcher", TOKEN).await?;
	services
		.users
		.create(user_id!("@directory_human:localhost"), Some("password"), None)
		.await?;
	services
		.appservice
		.register_appservice(serde_json::from_value(json!({
			"id": "directory-test",
			"url": null,
			"as_token": "directory-test-appservice-token-0123456789",
			"hs_token": "directory-test-homeserver-token-0123456789",
			"sender_localpart": "directory_sender",
			"namespaces": {
				"users": [{"exclusive": true, "regex": "^@directory_agent:localhost$"}],
				"aliases": [],
				"rooms": []
			}
		}))?)
		.await?;
	let agent = user_id!("@directory_agent:localhost");
	services.users.create(agent, None, None).await?;
	services
		.users
		.create_device(agent, None, (Some(AGENT_TOKEN), None), None, None, None)
		.await?;
	assert!(
		services
			.appservice
			.is_exclusive_user_id(agent)
			.await
	);

	let client = Client { services, base, token: TOKEN };
	assert_results(&client, "directory_", expected).await?;
	assert_results(&client, "no-matching-account", &[]).await?;
	assert_results(&client, requester.as_str(), &[]).await?;

	if show_appservices == Some(true) && !show_all {
		let agent_client = Client { services, base, token: AGENT_TOKEN };
		let room = agent_client
			.create_room(&json!({
				"preset": "private_chat", "invite": [requester]
			}))
			.await?;
		services
			.client
			.clients
			.default
			.post(client.url(&format!("rooms/{room}/join")))
			.bearer_auth(TOKEN)
			.json(&json!({}))
			.send()
			.await?
			.error_for_status()?;
		assert_results(&client, "directory_", &["@directory_agent:localhost"]).await?;
	}

	assert!(
		services
			.appservice
			.is_exclusive_user_id(agent)
			.await
	);
	Ok(())
}

async fn assert_results(client: &Client<'_>, term: &str, expected: &[&str]) -> Result {
	let body: Value = client
		.services
		.client
		.clients
		.default
		.post(client.url("user_directory/search"))
		.bearer_auth(client.token)
		.json(&json!({"search_term": term, "limit": 100}))
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;
	let mut users: Vec<_> = body["results"]
		.as_array()
		.expect("directory results")
		.iter()
		.map(|user| user["user_id"].as_str().expect("user ID"))
		.collect();
	users.sort_unstable();
	assert_eq!(users, expected, "directory account visibility for {term}");
	assert_eq!(body["limited"], false);
	Ok(())
}
