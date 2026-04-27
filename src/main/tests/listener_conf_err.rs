#![cfg(test)]

use insta::{assert_debug_snapshot, with_settings};
use tuwunel::{Args, Runtime, Server};

#[test]
#[should_panic = "There was a problem with your configuration"]
fn listener_conf_err() {
	with_settings!({
		description => "Listener Configuration Err",
		snapshot_suffix => "listener_conf_err",
	}, {
		let mut args = Args::default_test(&["smoke", "fresh", "cleanup"]);
		args.option.push("address=[\"256.256.256.256\"]".into());

		let runtime = Runtime::new(Some(&args)).unwrap();
		let server = Server::new(Some(&args), Some(&runtime)).unwrap();
		let result = tuwunel::exec(&server, runtime);

		assert_debug_snapshot!(result);
	});
}
