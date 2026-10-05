#![cfg(test)]

use std::net::TcpListener;

use insta::{assert_debug_snapshot, with_settings};
use reqwest::StatusCode;
use serde_json::Value;
use tuwunel::{Args, Runtime, Server};
use tuwunel_core::Result;

use self::fixture::boot;

#[expect(dead_code)] // Only the readiness probe is used from the client harness.
mod client;
mod fixture;

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
			.with_option(format!("port={port}"));

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;

		// the reservation ends here so the server can take the port
		drop(listener);

		let result = tuwunel::exec(&server, runtime);

		assert_debug_snapshot!(result);
		result
	})
}

#[test]
fn boots_with_federation_disabled() -> Result {
	boot(
		"federation-disabled",
		["allow_federation=false", "log_enable=false"],
		async |services, base| {
			let response = services
				.client
				.clients
				.default
				.get(format!("{base}/_matrix/federation/v1/version"))
				.send()
				.await?;

			assert_eq!(response.status(), StatusCode::FORBIDDEN);

			let body: Value = response.json().await?;

			assert_eq!(body["errcode"], "M_FORBIDDEN");
			assert_eq!(body["error"], "M_FORBIDDEN: Federation is disabled.");
			Ok(())
		},
	)
}
