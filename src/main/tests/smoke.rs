#![cfg(test)]

use std::net::TcpListener;

use insta::{assert_debug_snapshot, with_settings};
use reqwest::StatusCode;
use serde_json::Value;
use tuwunel::{Args, Runtime, Server};
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::fixture::boot;

#[expect(dead_code)] // Only the readiness probe is used from the client harness.
mod client;
mod fixture;

// Parallel boots share one tracing registry, which holds at most 4096 threads.
const POOL_CAP: &str = "database.pool.max_workers=64";

#[test]
fn dummy() {}

#[test]
#[should_panic = "dummy"]
fn panic_dummy() { panic!("dummy") }

#[test]
fn smoke() -> Result {
	with_settings!({
		description => "Smoke Test",
		snapshot_suffix => "smoke_test",
	}, {
		let listener = TcpListener::bind(("127.0.0.1", 0))?;
		let port = listener.local_addr()?.port();

		let args = Args::default_test(&["smoke", "fresh", "cleanup"])
			.with_option(format!("http.listen.port={port}"));

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;

		// the reservation ends here so the server can take the port
		drop(listener);

		let result = tuwunel::exec(&server, runtime);

		assert_debug_snapshot!(result);
		result
	})
}

/// An inbound federation request and a client request needing outbound
/// federation both answer 403 while federation is disabled.
///
/// The room summary request names a remote alias, so answering it needs a
/// federation query the server must refuse before any lookup or connection.
#[test]
fn boots_with_federation_disabled() -> Result {
	let options = ["federation.enable=false", "logging.enable=false", POOL_CAP];

	boot("federation-disabled", options, async |services, base| {
		let inbound = format!("{base}/_matrix/federation/v1/version");
		let outbound = format!("{base}/_matrix/client/v1/room_summary/%23room:remote.example");

		expect_federation_disabled(services, &inbound).await?;
		expect_federation_disabled(services, &outbound).await
	})
}

async fn expect_federation_disabled(services: &Services, url: &str) -> Result {
	let body = expect_error(services, url, StatusCode::FORBIDDEN, "M_FORBIDDEN").await?;

	assert_eq!(body["error"], "M_FORBIDDEN: Federation is disabled.", "{url}");
	Ok(())
}

/// An alias server that refuses the connection, and one that accepts it and
/// never answers, each read as a gateway error rather than a server fault.
///
/// The directory route still reports both as an unknown alias. The held
/// listener completes the TCP handshake from its backlog and leaves the TLS
/// hello unanswered, so the connect timeout fires.
#[test]
fn unreachable_alias_server_answers_as_a_gateway() -> Result {
	let options = [
		"logging.enable=false",
		"network.http.ip_range_denylist=[]",
		"network.http.conn_timeout=1",
		POOL_CAP,
	];

	boot("alias-unreachable", options, async |services, base| {
		let listener = TcpListener::bind(("127.0.0.1", 0))?;
		let silent = listener.local_addr()?.to_string();

		expect_alias_failure(services, base, "127.0.0.1:1").await?;
		expect_alias_failure(services, base, &silent).await
	})
}

async fn expect_alias_failure(services: &Services, base: &str, server: &str) -> Result {
	let summary = format!("{base}/_matrix/client/v1/room_summary/%23room:{server}");
	let directory = format!("{base}/_matrix/client/v3/directory/room/%23room:{server}");

	expect_error(services, &summary, StatusCode::BAD_GATEWAY, "M_CONNECTION_FAILED").await?;
	expect_error(services, &directory, StatusCode::NOT_FOUND, "M_NOT_FOUND").await?;

	Ok(())
}

async fn expect_error(
	services: &Services,
	url: &str,
	status: StatusCode,
	errcode: &str,
) -> Result<Value> {
	let response = services
		.client
		.clients
		.default
		.get(url)
		.send()
		.await?;

	assert_eq!(response.status(), status, "{url}");

	let body: Value = response.json().await?;

	assert_eq!(body["errcode"], errcode, "{url}");
	Ok(body)
}
