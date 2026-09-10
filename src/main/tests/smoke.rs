#![cfg(test)]

use std::net::TcpListener;

use insta::{assert_debug_snapshot, with_settings};
use tuwunel::{Args, Runtime, Server};
use tuwunel_core::Result;

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
