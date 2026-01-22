use tuwunel_core::{Err, Result};

use crate::{admin_command, admin_command_dispatch};

#[admin_command_dispatch]
#[derive(Debug, clap::Subcommand)]
pub(crate) enum TesterCommand {
	Panic,
	Failure,
	Tester,
	Timer,
}

#[rustfmt::skip]
#[admin_command]
async fn panic(&self) -> Result {

	panic!("panicked")
}

#[rustfmt::skip]
#[admin_command]
async fn failure(&self) -> Result {

	Err!("failed")
}

#[inline(never)]
#[rustfmt::skip]
#[admin_command]
async fn tester(&self) -> Result {

	self.write_str("Ok").await
}

#[inline(never)]
#[rustfmt::skip]
#[admin_command]
async fn timer(&self) -> Result {
	let started = std::time::Instant::now();
	timed(self.body);

	let elapsed = started.elapsed();
	self.write_str(&format!("completed in {elapsed:#?}")).await
}

#[inline(never)]
#[rustfmt::skip]
#[expect(unused_variables)]
fn timed(body: &[&str]) {

}
