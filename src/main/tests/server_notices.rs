#![cfg(test)]
use std::{env::temp_dir, fs::remove_dir_all, net::TcpListener, process::id as process_id};

use futures::{FutureExt, future::join};
use reqwest::{Method, RequestBuilder};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	matrix::Event,
	ruma::{EventId, UserId},
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const ADMIN_TOKEN: &str = "server-notices-admin-access-token";
const USER_TOKEN: &str = "server-notices-user-access-token-1";
const SERVER_TOKEN: &str = "server-notices-server-access-token";
const NOTICE: &str = "/_synapse/admin/v1/send_server_notice";
const TRANSACTION: &str = "/_synapse/admin/v1/send_server_notice/replay";

/// Exercise notice authorization and membership through the HTTP router.
///
/// One isolated server supplies both notice and ordinary-room fixtures, so
/// their leave behavior is checked against the same identity and account data.
#[test]
fn authorization_and_notice_membership() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let database = temp_dir()
		.join("tuwunel")
		.join(format!("server-notices-{}", process_id()));

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option(format!("database_path={database:?}"))
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("create_admin_room=true")
		.with_option("grant_admin_to_first_user=false")
		.with_option("registration_shared_secret=\"notices-test\"");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base)
				.boxed_local() // Layout cut for the local-only boot fixture.
				.await;

			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (running, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		running?;

		outcome
	});

	drop(runtime);
	remove_dir_all(database).ok();

	result
}

/// Establish distinct administrator and ordinary-user credentials.
///
/// Omitting the identity setting also exercises legacy `conduit` construction
/// and notice markers with the default server user.
async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;
	assert_eq!(services.globals.server_user.localpart(), "conduit");

	let admin = register(services, "notice-admin", ADMIN_TOKEN).await?;
	let user = register(services, "notice-user", USER_TOKEN).await?;

	services.admin.make_user_admin(&admin).await?;
	precedence(services, base).await?;

	let admin = Client { services, base, token: ADMIN_TOKEN };
	let user_client = Client { services, base, token: USER_TOKEN };
	let body = json!({
		"user_id": user,
		"content": {"msgtype": "m.text", "body": "notice"},
	})
	.to_string();

	let first = request(&admin, Method::POST, NOTICE, &body, 200).await?;
	let event = EventId::parse(field(&first, "event_id")?)?;
	let pdu = services.timeline.get_pdu(&event).await?;
	let room = pdu.room_id();
	let leave = format!("/_matrix/client/v3/rooms/{room}/leave");
	let join = format!("/_matrix/client/v3/rooms/{room}/join");

	assert!(services.state_cache.is_invited(&user, room).await);

	let rejected = request(&user_client, Method::POST, &leave, "{}", 403).await?;

	assert_eq!(rejected["errcode"], "M_CANNOT_LEAVE_SERVER_NOTICE_ROOM");
	request(&user_client, Method::POST, &join, "{}", 200).await?;
	request(&user_client, Method::POST, &leave, "{}", 200).await?;
	assert!(!services.state_cache.is_joined(&user, room).await);

	let sent = request(&admin, Method::PUT, TRANSACTION, &body, 200).await?;
	let replay = request(&admin, Method::PUT, TRANSACTION, &body, 200).await?;
	let event = EventId::parse(field(&sent, "event_id")?)?;
	let sent_pdu = services.timeline.get_pdu(&event).await?;

	assert_eq!(sent["event_id"], replay["event_id"]);
	assert_eq!(sent_pdu.room_id(), room);
	assert!(services.state_cache.is_invited(&user, room).await);
	ordinary_invite(&user_client, &user).await
}

/// Check that only notice routes defer body failures until authorization.
///
/// Each credential class is tested against both JSON syntax and missing-field
/// failures, while registration retains its ordinary extractor behavior.
async fn precedence(services: &Services, base: &str) -> Result {
	for (token, expected) in [("", 401), ("invalid", 401), (USER_TOKEN, 403), (ADMIN_TOKEN, 400)]
	{
		let client = Client { services, base, token };

		for (method, path) in [(Method::POST, NOTICE), (Method::PUT, TRANSACTION)] {
			body_cases(&client, method, path, expected).await?;
		}
	}

	let client = Client { services, base, token: "" };
	let response =
		request(&client, Method::POST, "/_matrix/client/v3/register", "{", 400).await?;

	assert_eq!(response["errcode"], "M_NOT_JSON");

	let response = request(&client, Method::GET, "/_synapse/admin/v1/register", "", 200).await?;

	assert_ne!(field(&response, "nonce")?, "");

	Ok(())
}

/// Exercises empty, incomplete, and malformed bodies for one credential class.
///
/// Forbidden responses must retain their Matrix error code as well as HTTP status.
async fn body_cases(client: &Client<'_>, method: Method, path: &str, expected: u16) -> Result {
	for body in ["", " ", "{}", "{"] {
		let response = request(client, method.clone(), path, body, expected).await?;

		if expected == 403 {
			assert_eq!(response["errcode"], "M_FORBIDDEN");
		}
	}

	Ok(())
}

/// Reject an ordinary invitation whose creator is the server identity.
///
/// The missing tag must classify this room as ordinary even though its creator
/// and joined server member satisfy the other notice-marker checks.
async fn ordinary_invite(client: &Client<'_>, user: &UserId) -> Result {
	let services = client.services;
	let server_user = &services.globals.server_user;

	services
		.users
		.create_device(server_user, None, (Some(SERVER_TOKEN), None), None, None, None)
		.await?;

	let server_client = Client {
		services,
		base: client.base,
		token: SERVER_TOKEN,
	};

	let room = server_client
		.create_room(&json!({"invite": [user]}))
		.await?;

	let leave = format!("/_matrix/client/v3/rooms/{room}/leave");

	assert!(services.state_cache.is_invited(user, &room).await);
	request(client, Method::POST, &leave, "{}", 200).await?;
	assert!(!services.state_cache.is_invited(user, &room).await);

	Ok(())
}

/// Send an optional-token request and retain its JSON for precise assertions.
///
/// The status assertion includes the response body so a routing or fixture
/// failure is distinguishable from the intended protocol rejection.
async fn request(
	client: &Client<'_>,
	method: Method,
	path: &str,
	body: &str,
	expected: u16,
) -> Result<Value> {
	let request = client
		.services
		.client
		.clients
		.default
		.request(method, format!("{}{path}", client.base))
		.header("Content-Type", "application/json")
		.body(body.to_owned());

	let response = Some(client.token)
		.filter(|token| !token.is_empty())
		.into_iter()
		.fold(request, RequestBuilder::bearer_auth)
		.send()
		.await?;

	let status = response.status().as_u16();
	let response: Value = response.json().await?;

	assert_eq!(status, expected, "{path}: {response}");

	Ok(response)
}
