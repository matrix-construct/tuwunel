use std::sync::atomic::Ordering;

use tuwunel::{Server, args, restart, runtime};
use tuwunel_core::{Result, debug_info};

fn main() -> Result {
	let args = args::parse();
	let runtime = runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(runtime.handle()))?;

	tuwunel::exec(&server, runtime)?;

	#[cfg(unix)]
	if server.server.restarting.load(Ordering::Acquire) {
		restart::restart();
	}

	debug_info!("Exit");
	Ok(())
}
