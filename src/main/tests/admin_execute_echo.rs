#![cfg(test)]

use insta::{assert_debug_snapshot, with_settings};
use tuwunel::{Args, Runtime, Server};
use tuwunel_core::Result;

#[test]
fn admin_execute_echo() -> Result {
	with_settings!({
		description => "Admin Execute Echo",
		snapshot_suffix => "admin_execute_echo",
	}, {
		let mut args = Args::default_test(&["smoke", "fresh", "cleanup"]);
		args.execute.push("debug echo Test".into());

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;
		let result = runtime.block_on(async {
			tuwunel::async_exec(&server).await
		});

		drop(runtime);
		assert_debug_snapshot!(result);
		result
	})
}
