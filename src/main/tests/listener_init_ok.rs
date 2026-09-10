#![cfg(test)]

use std::net::TcpListener;

use insta::{assert_debug_snapshot, with_settings};
use tokio::{
	select,
	time::{Duration, sleep},
};
use tuwunel::{Args, Runtime, Server};
use tuwunel_core::{Err, Result};

#[test]
fn listener_init_ok() -> Result {
	with_settings!({
		description => "Listener Initialization Ok",
		snapshot_suffix => "listener_init_ok",
	}, {
		let listener = TcpListener::bind(("127.0.0.1", 0))?;
		let port = listener.local_addr()?.port();

		let args = Args::default_test(&["fresh", "cleanup"])
			.with_option(format!("port={port}"));

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;

		// the reservation ends here so the server can take the port
		drop(listener);

		let result = runtime.block_on(async {
			select! {
				() = sleep(Duration::from_secs(5)) => Ok(()),
				_ = tuwunel::async_exec(&server) => Err!("Premature server shutdown"),
			}
		});

		assert_debug_snapshot!(result);
		result
	})
}
