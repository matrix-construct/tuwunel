#![cfg(test)]

use insta::{assert_debug_snapshot, with_settings};
use tuwunel::{Args, Server, runtime};
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
		let args = Args::default_test(&["smoke", "fresh", "cleanup"]);
		let runtime = runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(runtime.handle()))?;
		let result = tuwunel::exec(&server, runtime);

		assert_debug_snapshot!(result);
		result
	})
}
