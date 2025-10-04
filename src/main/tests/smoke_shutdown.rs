#![cfg(test)]

use insta::{assert_debug_snapshot, with_settings};
use tracing::Level;
use tuwunel::{Args, Server, runtime};
use tuwunel_core::{Result, utils::result::ErrLog};

#[test]
fn smoke_shutdown() -> Result {
	with_settings!({
		description => "Smoke Shutdown",
		snapshot_suffix => "smoke_shutdown",
	}, {
		let args = Args::default_test(&["fresh", "cleanup"]);
		let runtime = runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(runtime.handle()))?;
		let result = runtime.block_on(async {
			tuwunel::async_start(&server).await?;
			let run = tuwunel::async_run(&server);
			server.server.shutdown().log_err(Level::WARN).ok();
			run.await?;
			tuwunel::async_stop(&server).await
		});

		runtime::shutdown(&server, runtime)?;
		assert_debug_snapshot!(result);
		result
	})
}
