#![cfg(test)]

use tuwunel::Server;
use tuwunel_core::{Args, Result, runtime};

#[test]
fn dummy() {}

#[test]
#[should_panic = "dummy"]
fn panic_dummy() { panic!("dummy") }

#[test]
fn smoke() -> Result {
	let args = Args::default_test("smoke");
	let runtime = runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(runtime.handle()))?;

	tuwunel::exec(&server, runtime)
}
