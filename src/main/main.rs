use std::sync::atomic::Ordering;

use tuwunel::{Server, args, health::check, restart, runtime::Runtime};
use tuwunel_core::{Result, debug_info};

fn main() -> Result {
	let args = args::parse();
	if args.health_check {
		return check(&args);
	}

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	tuwunel::exec(&server, runtime)?;

	#[cfg(unix)]
	if server.server.restarting.load(Ordering::Acquire) {
		restart::restart();
	}

	debug_info!("Exit");
	Ok(())
}
