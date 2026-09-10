#![cfg(test)]

use std::net::TcpListener;

use insta::{assert_debug_snapshot, with_settings};
use tracing::Level;
use tuwunel::{Args, Runtime, Server};
use tuwunel_core::{Result, utils::result::ErrLog};

#[test]
fn smoke_shutdown() -> Result {
	with_settings!({
		description => "Smoke Shutdown",
		snapshot_suffix => "smoke_shutdown",
	}, {
		let listener = TcpListener::bind(("127.0.0.1", 0))?;
		let port = listener.local_addr()?.port();

		let args = Args::default_test(&["fresh", "cleanup"])
			.with_option(format!("port={port}"));

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;
		let result = runtime.block_on(async {
			tuwunel::async_start(&server).await?;

			// the reservation ends here so the server can take the port
			drop(listener);

			let run = tuwunel::async_run(&server);
			server.server.shutdown().log_err(Level::WARN).ok();
			run.await?;
			tuwunel::async_stop(&server).await
		});

		drop(runtime);
		assert_debug_snapshot!(result);
		result
	})
}
