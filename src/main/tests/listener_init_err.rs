#![cfg(test)]

use insta::{assert_debug_snapshot, with_settings};
use tokio::{
	select,
	time::{Duration, sleep},
};
use tuwunel::{Args, Runtime, Server};
use tuwunel_core::Err;

#[test]
#[should_panic = "I/O error: No such file or directory (os error 2)"]
fn listener_init_err() {
	with_settings!({
		description => "Listener Initialization Err",
		snapshot_suffix => "listener_init_err",
	}, {
		let mut args = Args::default_test(&["fresh", "cleanup"]);
		args.option.push("unix_socket_path=\"/non/existent/path\"".into());

		let runtime = Runtime::new(Some(&args)).unwrap();
		let server = Server::new(Some(&args), Some(&runtime)).unwrap();
		let result = runtime.block_on(async {
			select! {
				result = tuwunel::async_exec(&server) => result,
				() = sleep(Duration::from_secs(10)) => Err!("Shutdown hanging after error."),
			}
		});

		assert_debug_snapshot!(result);
		result.unwrap();
	});
}
