#![cfg(test)]

use insta::{assert_debug_snapshot, with_settings};
use tuwunel::{Args, Server, runtime};
use tuwunel_core::Result;

#[test]
fn smoke_async() -> Result {
	with_settings!({
		description => "Smoke Async",
		snapshot_suffix => "smoke_async",
	}, {
		let args = Args::default_test(&["smoke", "fresh", "cleanup"]);
		let runtime = runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(runtime.handle()))?;
		let result = runtime.block_on(async {
			tuwunel::async_exec(&server).await
		});

		runtime::shutdown(&server, runtime)?;
		assert_debug_snapshot!(result);
		result
	})
}
