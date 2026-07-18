use std::sync::atomic::Ordering;

use tuwunel::{Server, args, health::check, restart, runtime::Runtime};
use tuwunel_core::{Result, debug_info};

// Bionic rejects an under-aligned PT_TLS segment on arm64.
#[cfg(all(
	target_os = "android",
	any(target_arch = "aarch64", target_arch = "arm")
))]
core::arch::global_asm!(
	".pushsection .tdata.tuwunel_tls_align,\"awTR\",%progbits",
	".p2align 6",
	"__tuwunel_tls_align:",
	".zero 64",
	".popsection",
);

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
