#![cfg(test)]

use std::net::TcpListener;

use insta::{assert_debug_snapshot, with_settings};
use tuwunel::{Args, Runtime, Server};
use tuwunel_core::Result;

#[test]
fn listener_conf_ok() -> Result {
	with_settings!({
		description => "Listener Configuration Ok",
		snapshot_suffix => "listener_conf_ok",
	}, {
		// this test configures the wildcard, where a failed bind is fatal, not skipped
		let listener = TcpListener::bind(("0.0.0.0", 0))?;
		let port = listener.local_addr()?.port();

		let args = Args::default_test(&["smoke", "fresh", "cleanup"])
			.with_option("address=[\"0.0.0.0\"]")
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
