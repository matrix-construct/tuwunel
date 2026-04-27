#![cfg(test)]

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
		let args = Args::default_test(&["fresh", "cleanup"]);

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;
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
