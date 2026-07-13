use std::sync::Arc;

use tokio::signal;
use tuwunel_core::{debug_error, trace, warn};

use super::server::Server;

#[cfg(unix)]
#[tracing::instrument(skip_all)]
pub async fn enable(server: Arc<Server>) {
	use signal::unix;
	use unix::SignalKind;

	const CONSOLE: bool = cfg!(feature = "console");
	const RELOADING: bool = cfg!(all(tuwunel_mods, feature = "tuwunel_mods", not(CONSOLE)));

	let mut quit = unix::signal(SignalKind::quit()).expect("SIGQUIT handler");
	let mut term = unix::signal(SignalKind::terminate()).expect("SIGTERM handler");
	let mut usr1 = unix::signal(SignalKind::user_defined1()).expect("SIGUSR1 handler");
	let mut usr2 = unix::signal(SignalKind::user_defined2()).expect("SIGUSR2 handler");
	loop {
		trace!("Installed signal handlers");
		let sig: &'static str;
		tokio::select! {
			() = server.server.until_shutdown() => break,
			_ = signal::ctrl_c() => { sig = "SIGINT"; },
			_ = quit.recv() => { sig = "SIGQUIT"; },
			_ = term.recv() => { sig = "SIGTERM"; },
			_ = usr1.recv() => { sig = "SIGUSR1"; },
			_ = usr2.recv() => { sig = "SIGUSR2"; },
		}

		warn!("Received {sig}");
		let result = if RELOADING && sig == "SIGINT" {
			server.server.reload()
		} else if matches!(sig, "SIGQUIT" | "SIGTERM") || (!CONSOLE && sig == "SIGINT") {
			server.server.shutdown()
		} else {
			server.server.signal(sig)
		};

		if let Err(e) = result {
			debug_error!(?sig, "signal: {e}");
		}
	}
}

#[cfg(not(unix))]
#[tracing::instrument(skip_all)]
pub async fn enable(server: Arc<Server>) {
	loop {
		tokio::select! {
			() = server.server.until_shutdown() => break,
			_ = signal::ctrl_c() => {
				warn!("Received Ctrl+C");
				if let Err(e) = server.server.shutdown() {
					debug_error!("shutdown: {e}");
				}
			},
		}
	}
}
