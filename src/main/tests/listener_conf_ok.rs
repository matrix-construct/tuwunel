#![cfg(test)]

use insta::{assert_debug_snapshot, with_settings};
use tuwunel::{Args, Runtime, Server};
use tuwunel_core::Result;

#[test]
fn listener_conf_ok() -> Result {
	with_settings!({
		description => "Listener Configuration Ok",
		snapshot_suffix => "listener_conf_ok",
	}, {
		let mut args = Args::default_test(&["smoke", "fresh", "cleanup"]);
		args.option.push("address=[\"0.0.0.0\"]".into());

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;
		let result = tuwunel::exec(&server, runtime);

		assert_debug_snapshot!(result);
		result
	})
}
